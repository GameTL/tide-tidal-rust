//! Pinning rearranges a grid page in place: the pinned card flies from where
//! its menu was opened to the Pinned group, and the cards that stay glide to
//! their new slots, in 250ms. Cards are located by clicking (the card answers
//! with App.open-card), the way the user finds them.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};

slint::include_modules!();

fn tick(ms: u64) {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(ms));
}

fn card(id: &str) -> CardData {
    CardData {
        id: id.into(),
        kind: "album".into(),
        title: format!("Album {id}").into(),
        subtitle: "Artist".into(),
        ..Default::default()
    }
}

fn group(title: &str, cards: Vec<CardData>) -> (SectionData, Rc<VecModel<CardData>>) {
    let model = Rc::new(VecModel::from(cards));
    (
        SectionData {
            title: title.into(),
            layout: "cards".into(),
            cards: ModelRc::from(model.clone()),
            tracks: ModelRc::default(),
        },
        model,
    )
}

/// Which card answers a click at (x, y), if any.
fn hit(win: &slint::Window, opened: &RefCell<Vec<String>>, x: f32, y: f32) -> Option<String> {
    let position = LogicalPosition::new(x, y);
    opened.borrow_mut().clear();
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
    win.dispatch_event(WindowEvent::PointerExited);
    opened.borrow().last().cloned()
}

/// A point just inside each card's top-left corner (clear of the artwork's
/// buttons), found by clicking a coarse grid of points.
fn centres(win: &slint::Window, opened: &RefCell<Vec<String>>) -> HashMap<String, (f32, f32)> {
    let mut boxes: HashMap<String, (f32, f32, f32, f32)> = HashMap::new();
    for y in (200..800).step_by(12) {
        for x in (250..1100).step_by(12) {
            if let Some(id) = hit(win, opened, x as f32, y as f32) {
                let b = boxes.entry(id).or_insert((f32::MAX, f32::MAX, 0., 0.));
                *b = (b.0.min(x as f32), b.1.min(y as f32), b.2.max(x as f32), b.3.max(y as f32));
            }
        }
    }
    boxes
        .into_iter()
        .map(|(id, (x0, y0, _, _))| (id, (x0 + 12., y0 + 12.)))
        .collect()
}

#[test]
fn pinning_flies_the_card_to_the_top_and_the_rest_glide() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized { size: LogicalSize::new(1440., 900.) });
    app.set_logged_in(true);
    app.set_view("grid".into());
    app.set_grid_kind("albums".into());
    app.set_page_title("Albums".into());
    // The smallest step: several cards to a row.
    app.set_grid_size(0);

    let opened = Rc::new(RefCell::new(Vec::<String>::new()));
    for record in [0, 1] {
        let opened = opened.clone();
        let f = move |c: CardData| opened.borrow_mut().push(c.id.to_string());
        if record == 0 {
            app.on_open_card(f);
        } else {
            app.on_play_card(f);
        }
    }

    let ids = ["a", "b", "c", "d", "e"];
    let (rest, rest_cards) = group("", ids.iter().map(|id| card(id)).collect());
    let outer = Rc::new(VecModel::from(vec![rest]));
    app.set_grid_cards(ModelRc::new(VecModel::from(ids.iter().map(|id| card(id)).collect::<Vec<_>>())));
    app.set_grid_groups(ModelRc::from(outer.clone()));
    tick(500);

    let before = centres(win, &opened);
    assert_eq!(before.len(), 5, "every card is on screen: {before:?}");

    // Opening the menu on "c" notes where it sits; then Pin.
    app.set_menu_kind("album".into());
    app.set_menu_card(card("c"));
    app.set_menu_open(true);
    tick(1);
    app.set_menu_open(false);
    tick(1);

    // What the controller does on Pin (reflow_grid): patch in place.
    app.set_grid_reflow(true);
    rest_cards.remove(2);
    let (pinned, _) = group("Pinned", vec![card("c")]);
    outer.insert(0, pinned);
    app.set_grid_fly_group(0);
    tick(1);

    // Right after: everything is still drawn where it was.
    for id in ["a", "c", "e"] {
        let (x, y) = before[id];
        assert_eq!(hit(win, &opened, x, y).as_deref(), Some(id), "{id} starts where it was");
    }

    tick(300);
    app.set_grid_reflow(false);
    tick(1);
    let after = centres(win, &opened);
    assert_eq!(after.len(), 5, "{after:?}");
    // "c" leads the page now, above every card that is not pinned.
    for id in ["a", "b", "d", "e"] {
        assert!(after["c"].1 < after[id].1, "c above {id}: {after:?}");
    }
    // The rest moved down under the Pinned group and closed the gap.
    assert!(after["a"].1 > before["a"].1);
    assert_eq!(after["d"].0, before["c"].0, "d took c's column");

    // Unpin: back where it came from.
    let (outer2, cards2) = (outer.clone(), rest_cards.clone());
    app.set_menu_card(card("c"));
    app.set_menu_open(true);
    tick(1);
    app.set_menu_open(false);
    tick(1);
    app.set_grid_reflow(true);
    outer2.remove(0);
    cards2.insert(2, card("c"));
    app.set_grid_fly_group(0);
    // Frame by frame, as the window would draw it.
    for _ in 0..5 {
        tick(25);
    }
    let mid = centres(win, &opened);
    let between = |id: &str| {
        let (lo, hi) = (before[id].1.min(after[id].1), before[id].1.max(after[id].1));
        mid[id].1 > lo + 6. && mid[id].1 < hi - 6.
    };
    assert!(between("a") && between("c"), "half way back at 125ms: {mid:?}");
    for _ in 0..12 {
        tick(25);
    }
    app.set_grid_reflow(false);
    tick(1);
    let back = centres(win, &opened);
    for id in ids {
        let (x, y) = back[id];
        let (x0, y0) = before[id];
        assert!((x - x0).abs() < 13. && (y - y0).abs() < 13., "{id} back in place: {back:?}");
    }
}
