// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use clru::CLruCache;
use i_slint_common::sharedfontique::HashedBlob;
use i_slint_core::textlayout::sharedparley::{fontique, parley};
use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;

const FONT_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(64).unwrap();

pub struct FontCache {
    font_mgr: skia_safe::FontMgr,
    // Use HashedBlob in key to keep strong reference to font data blob,
    // preventing eviction from fontique's shared cache (see commit 30a03cf).
    // The u64 is a hash of variation settings (0 for base typefaces).
    fonts: CLruCache<(HashedBlob, u32, u64), Option<skia_safe::Typeface>>,
    // TIDE patch: font files handed to Skia without copying them, by blob id.
    // See `load_typeface_internal`.
    pinned: std::collections::HashMap<u64, &'static fontique::Blob<u8>>,
}

impl Default for FontCache {
    fn default() -> Self {
        Self {
            font_mgr: skia_safe::FontMgr::new(),
            fonts: CLruCache::new(FONT_CACHE_CAPACITY),
            pinned: Default::default(),
        }
    }
}

impl FontCache {
    pub fn font_with_variations(
        &mut self,
        font: &parley::FontData,
        synthesis: &fontique::Synthesis,
    ) -> Option<skia_safe::Typeface> {
        let variation_settings = synthesis.variation_settings();

        let mut variations_hash = 0u64;
        if !variation_settings.is_empty() {
            let mut hasher = DefaultHasher::new();
            for &(tag, value) in variation_settings {
                tag.to_be_bytes().hash(&mut hasher);
                value.to_bits().hash(&mut hasher);
            }
            variations_hash = hasher.finish();
        }

        let key = (font.data.clone().into(), font.index, variations_hash);

        if let Some(cached) = self.fonts.get(&key) {
            return cached.clone();
        }

        let mut typeface = self.load_typeface_internal(font);

        if !variation_settings.is_empty() {
            typeface = typeface.and_then(|base| {
                let coords: Vec<skia_safe::font_arguments::variation_position::Coordinate> =
                    variation_settings
                        .iter()
                        .map(|&(tag, value)| {
                            skia_safe::font_arguments::variation_position::Coordinate {
                                axis: skia_safe::FourByteTag::new(u32::from_be_bytes(
                                    tag.to_be_bytes(),
                                )),
                                value,
                            }
                        })
                        .collect();
                let position =
                    skia_safe::font_arguments::VariationPosition { coordinates: &coords };
                let args = skia_safe::FontArguments::new().set_variation_design_position(position);
                base.clone_with_arguments(&args).or(Some(base))
            });
        }

        self.fonts.put(key, typeface.clone());
        typeface
    }

    fn load_typeface_internal(&mut self, font: &parley::FontData) -> Option<skia_safe::Typeface> {
        // TIDE patch: upstream copies the whole font file onto the heap here
        // (`Data::new_copy`). System fonts are memory-mapped, and Apple Color
        // Emoji alone is about 190 MB, so one emoji cost that much RAM. Hand
        // Skia the mapped bytes instead; one reference to the blob is kept
        // for the life of the process so the mapping never goes away under a
        // typeface that outlives the cache above.
        let blob: &'static fontique::Blob<u8> = self
            .pinned
            .entry(font.data.id())
            .or_insert_with(|| Box::leak(Box::new(font.data.clone())));
        // SAFETY: `blob` is never dropped, so its bytes outlive the `Data`.
        let data = unsafe { skia_safe::Data::new_bytes(blob.as_ref()) };
        let typeface = self
            .font_mgr
            .new_from_data(data, if font.index > 0 { Some(font.index as _) } else { None });

        // TIDE patch: the workaround below rebuilds the face on the heap,
        // which is some 55 MB for one face of a CJK system collection. When
        // the same face is installed, CoreText can provide it without a copy.
        #[cfg(target_vendor = "apple")]
        if font.index > 0
            && typeface.is_none()
            && let Some(typeface) = self.installed_face(font)
        {
            return Some(typeface);
        }

        // Due to  https://issues.skia.org/issues/310510989, fonts from true type collections
        // with an index > 0 fail to load on macOS. As a workaround, we manually extract the font from the
        // collection and load it as a single font.
        #[cfg(target_vendor = "apple")]
        if font.index > 0
            && typeface.is_none()
            && let Some(typeface) = read_fonts::CollectionRef::new(font.data.as_ref())
                .ok()
                .and_then(|ttc| ttc.get(font.index).ok())
                .map(|ttf| write_fonts::FontBuilder::new().copy_missing_tables(ttf).build())
                .and_then(|new_ttf| {
                    self.font_mgr.new_from_data(skia_safe::Data::new_copy(&new_ttf), None)
                })
        {
            return Some(typeface);
        }

        typeface
    }
}

#[cfg(target_vendor = "apple")]
impl FontCache {
    /// TIDE patch: the installed font that is this very face of a collection,
    /// looked up by name. Glyph ids must match the data the text was shaped
    /// with, so anything but the same PostScript name and glyph count is
    /// turned down.
    fn installed_face(&self, font: &parley::FontData) -> Option<skia_safe::Typeface> {
        use read_fonts::TableProvider;
        use read_fonts::types::NameId;

        let face = read_fonts::FontRef::from_index(font.data.as_ref(), font.index).ok()?;
        let names = face.name().ok()?;
        // Every spelling of a name: a face can carry localized family names.
        let all = |id: NameId| {
            let mut found = Vec::<String>::new();
            for record in names.name_record().iter().filter(|record| record.name_id() == id) {
                if let Ok(text) = record.string(names.string_data()) {
                    let text = text.chars().collect::<String>();
                    if !text.is_empty() && !found.contains(&text) {
                        found.push(text);
                    }
                }
            }
            found
        };
        let postscript = all(NameId::POSTSCRIPT_NAME).into_iter().next()?;
        let glyphs = face.maxp().ok()?.num_glyphs() as usize;
        let (weight, width, italic) = face.os2().ok().map_or((400, 5, false), |os2| {
            (
                os2.us_weight_class() as i32,
                os2.us_width_class() as i32,
                os2.fs_selection().contains(read_fonts::tables::os2::SelectionFlags::ITALIC),
            )
        });
        let slant = if italic {
            skia_safe::font_style::Slant::Italic
        } else {
            skia_safe::font_style::Slant::Upright
        };
        // Some collections declare the same weight for every face, so the
        // declared one is only the first to try.
        let weights = std::iter::once(weight)
            .chain((1..=9).map(|step| step * 100).filter(move |candidate| *candidate != weight));
        let families: Vec<String> =
            [NameId::TYPOGRAPHIC_FAMILY_NAME, NameId::FAMILY_NAME].into_iter().flat_map(all).collect();
        for weight in weights {
            let style = skia_safe::FontStyle::new(
                skia_safe::font_style::Weight::from(weight),
                skia_safe::font_style::Width::from(width),
                slant,
            );
            let found = families
                .iter()
                .filter_map(|family| self.font_mgr.match_family_style(family, style))
                .find(|typeface| {
                    typeface.post_script_name().as_deref() == Some(postscript.as_str())
                        && typeface.count_glyphs() == glyphs
                });
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

thread_local! {
    pub static FONT_CACHE: RefCell<FontCache> = RefCell::new(Default::default())
}
