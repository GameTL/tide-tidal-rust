//! Slim items (Home shortcuts and feed rows) behave the same for every kind:
//! the artwork plays, the rest of the row opens (a track plays), right-click
//! opens the item's menu.

use std::cell::RefCell;
use std::rc::Rc;

use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, ModelRc, VecModel};

slint::include_modules!();

fn click(win: &slint::Window, x: f32, y: f32, button: PointerEventButton) {
    let position = LogicalPosition::new(x, y);
    win.dispatch_event(WindowEvent::PointerMoved { position });
    win.dispatch_event(WindowEvent::PointerPressed { position, button });
    win.dispatch_event(WindowEvent::PointerReleased { position, button });
}

fn card(id: &str, kind: &str) -> CardData {
    CardData {
        id: id.into(),
        kind: kind.into(),
        title: id.into(),
        subtitle: "Created by me".into(),
        ..Default::default()
    }
}

#[derive(Default)]
struct Calls {
    played: Vec<(String, String)>,
    opened: Vec<(String, String)>,
    menus: Vec<(String, String)>,
}

#[test]
fn every_slim_item_plays_from_its_artwork_and_opens_from_its_row() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new().unwrap();
    let app = ui.global::<App>();
    let win = ui.window();
    ui.show().unwrap();
    win.dispatch_event(WindowEvent::Resized {
        size: LogicalSize::new(1440., 900.),
    });
    app.set_logged_in(true);
    app.set_view("home".into());
    let section = |title: &str, layout: &str, cards: Vec<CardData>, tracks: Vec<TrackData>| {
        SectionData {
            title: title.into(),
            layout: layout.into(),
            cards: ModelRc::new(VecModel::from(cards)),
            tracks: ModelRc::new(VecModel::from(tracks)),
        }
    };
    app.set_sections(ModelRc::new(VecModel::from(vec![
        section("", "shortcuts", vec![card("fuel", "playlist")], vec![]),
        section(
            "Recently played",
            "rows",
            vec![card("aces", "album")],
            vec![TrackData {
                id: "famous".into(),
                title: "Famous".into(),
                artist: "Kanye West".into(),
                explicit: true,
                ..Default::default()
            }],
        ),
    ])));

    let calls = Rc::new(RefCell::new(Calls::default()));
    let key = |c: &CardData| (c.kind.to_string(), c.id.to_string());
    app.on_play_card({
        let calls = calls.clone();
        move |c| calls.borrow_mut().played.push(key(&c))
    });
    app.on_open_card({
        let calls = calls.clone();
        move |c| calls.borrow_mut().opened.push(key(&c))
    });
    app.on_request_card_menu({
        let calls = calls.clone();
        move |c, _, _| calls.borrow_mut().menus.push(key(&c))
    });

    // Pane geometry as HomeView lays it out: side padding, then a 3-column grid.
    win.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(1., 1.),
    });
    let (pane_x, pane_y, pane_w) = (app.get_pane_x(), app.get_pane_y(), app.get_pane_width());
    let pad = ((pane_w - 900.) * 0.11).clamp(24., 83.);
    let x0 = pane_x + pad;
    let rows_col = (pane_w - 2. * pad - 2. * 30.) / 3.;

    // Find each item's row by sweeping down a column of artwork: the y range
    // whose clicks play it.
    let sweep = |x: f32| -> Vec<(String, f32)> {
        let mut hits: Vec<(String, f32, f32)> = Vec::new();
        let mut y = pane_y;
        while y < pane_y + 600. {
            let before = calls.borrow().played.len();
            click(win, x, y, PointerEventButton::Left);
            if calls.borrow().played.len() > before {
                let id = calls.borrow().played.last().unwrap().1.clone();
                match hits.last_mut() {
                    Some(h) if h.0 == id => h.2 = y,
                    _ => hits.push((id, y, y)),
                }
            }
            y += 3.;
        }
        hits.into_iter().map(|(id, a, b)| (id, (a + b) / 2.)).collect()
    };
    let mut found = sweep(x0 + 9. + 20.);
    let second = sweep(x0 + rows_col + 30. + 20.);
    found.extend(second);
    let ids: Vec<&str> = found.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        ["fuel", "aces", "famous"],
        "the artwork of the shortcut, the album row and the track row each play"
    );
    let xs = [x0, x0, x0 + rows_col + 30.];
    let kinds = ["playlist", "album", "track"];
    *calls.borrow_mut() = Calls::default();

    for (((id, y), x), kind) in found.iter().zip(xs).zip(kinds) {
        let want = (kind.to_string(), id.clone());
        let body_x = x + 160.;

        click(win, x + 29., *y, PointerEventButton::Left);
        assert_eq!(calls.borrow().played.last(), Some(&want), "artwork of {id} plays");

        click(win, body_x, *y, PointerEventButton::Left);
        assert_eq!(calls.borrow().opened.last(), Some(&want), "row of {id} opens");

        click(win, body_x, *y, PointerEventButton::Right);
        assert_eq!(calls.borrow().menus.last(), Some(&want), "right-click on {id} opens its menu");
    }
    let calls = calls.borrow();
    assert_eq!(calls.played.len(), 3);
    assert_eq!(calls.opened.len(), 3);
    assert_eq!(calls.menus.len(), 3);
}
