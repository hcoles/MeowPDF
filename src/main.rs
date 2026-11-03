mod drivers;
use crate::drivers::commands::ClearImages;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, window_size, Clear, ClearType,
    EnterAlternateScreen, LeaveAlternateScreen, WindowSize,
};
use drivers::commands::{
    DisableMouseCapturePixels, EnableMouseCapturePixels, PointerShape, SetPointerShape,
};
use drivers::graphics::terminal_graphics_test_support;
use keybinds::{KeyInput, Keybinds};

mod threads;
use threads::event::InputEvent;

mod image;
use crate::image::*;

mod toc;
use crate::toc::*;

mod viewer;
use crate::viewer::*;

mod globals;
use crate::globals::*;

mod config;
use crate::config::*;

use std::hash::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};
use threads::audiosync::{SyncData, SyncUpdate};

/* Tracks the last executed times of signals for throattling */
/* State of following an audio player */
struct Follow {
    data: Option<Arc<SyncData>>,
    enabled: bool,
    cue: Option<usize>,
}

impl Follow {
    fn apply(&self, viewer: &mut Viewer) {
        let region = self
            .data
            .as_ref()
            .zip(self.cue)
            .and_then(|(d, c)| d.cues[c].regions.first());
        if let Some(r) = region {
            if r.page >= 1 {
                viewer.scroll_to_region(r.page - 1, r.y, r.height);
            }
        }
    }
}

/* Reads the audio sync data embedded in the PDF, if any */
fn load_sync(pdf: &str) -> Result<Option<Arc<SyncData>>, String> {
    Ok(SyncData::load(Path::new(pdf))?.map(Arc::new))
}

struct LastExecuted {
    pub load: SystemTime,
    pub alpha: SystemTime,
    pub inverse: SystemTime,
}

fn main() {
    /* ============================= Check the argument ============================= */
    let arg = std::env::args().nth(1).unwrap_or("".to_owned());
    match arg.as_str() {
        "-h" | "--help" | "" => {
            println!("{}", HELP_MSG);
            return;
        }
        "-v" | "--version" => {
            println!("meowpdf v{} ({})", VERSION, RELEASED);
            return;
        }
        _ => (),
    }

    let sync = match load_sync(&arg) {
        Ok(sync) => sync,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    /* ============================= Uncook the terminal ============================= */
    enable_raw_mode().expect("Could not cook the terminal");
    execute!(io::stdout(), EnterAlternateScreen).expect("Could not enter alt mode");
    execute!(io::stdout(), Hide).expect("Could not hide cursor");
    execute!(io::stdout(), Clear(ClearType::All)).expect("Could not clear terminal");
    execute!(io::stdout(), EnableMouseCapturePixels)
        .expect("Could not enable mouse capture");

    /* ========================== Cook the terminal on panic ========================= */
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        /* Atleast try to cook the terminal on error before printing the message.
         * Do not handle the error to prevent possible infinite loops when panicking. */

        let _ = execute!(io::stdout(), DisableMouseCapturePixels);
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = execute!(io::stdout(), Show);
        let _ = disable_raw_mode();
        default_panic(info);
    }));

    /* ============================= STDIN parser thread ============================= */
    let event_inputs = threads::event::spawn();
    RECEIVER_GR.get_or_init(|| Mutex::new(event_inputs.2));

    /* ========== Check if the terminal supports the Kitty graphics protocol ========= */
    terminal_graphics_test_support()
        .expect("Error when testing terminal support of the Kitty graphics protocol");

    /* ================================= Load config ================================= */
    let mut key_matcher;
    {
        let mut config = config_load_or_create().expect("Could not load config");
        key_matcher = config.bindings.unwrap();
        config.bindings = None;
        CONFIG.get_or_init(|| config);
    }

    /* ======================= Calculate padding for all images ====================== */
    let winsize_tmp = window_size().expect("Could not get win size");
    let winsize = WindowSize {
        rows: winsize_tmp.rows,
        columns: winsize_tmp.columns,
        width: winsize_tmp.width,
        height: winsize_tmp.height,
    };
    TERMINAL_SIZE.get_or_init(|| RwLock::new(winsize_tmp));

    let config = CONFIG.get().unwrap();

    let pxpercol = winsize.width as f64 / winsize.columns as f64;
    let pxperrow = winsize.height as f64 / winsize.rows as f64;

    let paddingcol = (pxpercol * config.viewer.render_precision
        / config.viewer.scale_min as f64)
        .ceil() as usize;
    let paddingrow = (pxperrow * config.viewer.render_precision
        / config.viewer.scale_min as f64)
        .ceil() as usize;

    IMAGE_PADDING.get_or_init(|| std::cmp::max(paddingcol, paddingrow));

    /* =========== Generate a random ID which is unique for every instance =========== */
    let random_u64 = RandomState::new().build_hasher().finish();
    SOFTWARE_ID.get_or_init(|| format!("{random_u64:X}"));

    /* ====================== Viewer - The core of this program ====================== */
    let (mut viewer, sender_rerender) = Viewer::new();

    let (mut renderer, result_receiver) = threads::renderer::Renderer::new();
    renderer.run(&arg).expect("Couldn't start renderer thread");
    renderer
        .send_and_confirm_action(threads::renderer::RendererAction::Load)
        .expect("Cannot send action to renderer thread");

    /* ========================= Thread notifying file change ======================== */
    let file_reload = threads::fnotify::spawn(&arg).expect("Could not init file watcher");

    /* ============================== Main program loop ============================== */
    let mut throttle_data = LastExecuted {
        load: SystemTime::now() - Duration::from_millis(500),
        alpha: SystemTime::now() - Duration::from_millis(500),
        inverse: SystemTime::now() - Duration::from_millis(500),
    };

    let mut current_mouse = MouseEvent {
        kind: MouseEventKind::Moved,
        column: u16::MAX,
        row: u16::MAX,
        modifiers: KeyModifiers::NONE,
    };

    let mut toc = Toc::new();

    let sync_updates = match &sync {
        Some(data) => threads::audiosync::spawn(data.clone()),
        None => crossbeam_channel::never(),
    };
    let mut follow = Follow {
        data: sync,
        enabled: true,
        cue: None,
    };

    'main: loop {
        let toc_was_open = toc.open;

        /* sel[0..1] are the results from the renderer thread */
        let mut sel = result_receiver.construct_biased_select();
        sel.recv(&file_reload);
        sel.recv(&sender_rerender);
        /* Key event */
        sel.recv(&event_inputs.0);
        /* Mouse event */
        sel.recv(&event_inputs.1);
        /* Window size change input */
        sel.recv(&event_inputs.3);
        /* Audio position updates */
        sel.recv(&sync_updates);

        /* Audio position updates must not starve behind a stream of other events */
        let index_ready = if sync_updates.is_empty() {
            sel.ready()
        } else {
            7
        };

        /* Mouse movement must not trigger a redraw of the table of contents */
        if toc.open && index_ready == 5 {
            current_mouse = event_inputs.1.try_recv().expect("Could not receive mouse");
            continue;
        }

        execute!(io::stdout(), ClearImages, Clear(ClearType::FromCursorDown))
            .expect("Could not clear images");

        match index_ready {
            0 | 1 => {
                let result = result_receiver
                    .try_recv_priority(index_ready)
                    .expect("Could not receive priority");

                match result {
                    threads::renderer::RendererResult::PageMetadata {
                        max_page_width,
                        cumulative_heights,
                        widths,
                        links,
                        toc: toc_entries,
                    } => {
                        let uninit = viewer.is_uninit();

                        viewer.update_metadata(
                            max_page_width,
                            &cumulative_heights,
                            &widths,
                            &links,
                        );
                        toc.set_entries(toc_entries);
                        viewer.invalidate_registry();
                        viewer.center_viewer();
                        if uninit {
                            viewer.scale_page2terminal();
                        }
                        result_receiver.clear_priority(1);
                    }
                    threads::renderer::RendererResult::Image { page, data } => {
                        viewer.handle_image(page, data);
                    }
                }
            }
            2 => {
                file_reload
                    .try_recv()
                    .expect("Could not receive file reload");
                if throttle_data.load.elapsed().unwrap() >= Duration::from_millis(1000) {
                    renderer
                        .send_and_confirm_action(threads::renderer::RendererAction::Load)
                        .expect("Cannot send action to renderer thread");
                }
            }
            3 => {
                sender_rerender
                    .try_recv()
                    .expect("Could not receive rerender");
            }
            4 => {
                let input = event_inputs.0.try_recv().expect("Could not receive input");
                match input {
                    InputEvent::Key(key) if toc.open => {
                        if let Some(page) = toc.handle_key(key) {
                            let _ = viewer.jump(page);
                        }
                    }
                    InputEvent::Key(key) => {
                        if handle_key(
                            key,
                            &mut key_matcher,
                            &mut viewer,
                            &renderer,
                            &mut throttle_data,
                            &mut toc,
                            &mut follow,
                        ) {
                            break 'main;
                        }
                    }
                    InputEvent::MouseScroll(kind, _modifiers) => {
                        if handle_mouse_scroll(kind, &mut viewer) {
                            break 'main;
                        }
                    }
                }
            }
            5 => {
                current_mouse =
                    event_inputs.1.try_recv().expect("Could not receive mouse");
            }
            6 => {
                let (width, height) = event_inputs
                    .3
                    .try_recv()
                    .expect("Could not receive from win-size");

                let old_width;
                {
                    let mut handle = TERMINAL_SIZE
                        .get()
                        .unwrap()
                        .write()
                        .expect("Could not get win sie handle");
                    old_width = handle.width;
                    handle.width = width;
                    handle.height = height;

                    /* Rows and columns are needed to calculate the pixel size of
                     * a cell and must be kept in sync with the pixel size */
                    if let Ok(size) = window_size() {
                        handle.rows = size.rows;
                        handle.columns = size.columns;
                        handle.width = size.width;
                        handle.height = size.height;
                    }
                }

                viewer.resize(old_width);
            }
            7 => {
                let SyncUpdate {
                    mut cue,
                    mut resume,
                } = sync_updates
                    .try_recv()
                    .expect("Could not receive audio sync update");
                /* Only the latest position matters */
                while let Ok(next) = sync_updates.try_recv() {
                    cue = next.cue;
                    resume |= next.resume;
                }
                follow.cue = Some(cue);
                if let Some(data) = &follow.data {
                    viewer.set_highlight(data.cues[cue].regions.clone());
                }
                if resume {
                    follow.enabled = true;
                }
                if follow.enabled {
                    follow.apply(&mut viewer);
                }
            }
            _ => unreachable!(),
        };

        /* The user took over the view. Index 7 never sets this flag. */
        if viewer.take_manual_move() {
            follow.enabled = false;
        }

        if toc.open {
            toc.draw();
            continue;
        } else if toc_was_open {
            Toc::erase();
        }

        if let Some(link) = viewer.intersect_link(current_mouse) {
            execute!(io::stdout(), SetPointerShape(PointerShape::Pointer))
                .expect("Could not set pointer shape");

            viewer.uri_hint(&link);
            if current_mouse.kind.is_down() {
                /* URI points to page in this document */
                if link.uri.starts_with('#') {
                    if let Some(destination) = &link.dest {
                        let _ = viewer.jump(destination.loc.page_number as usize);
                    }
                } else {
                    let _ = open::that_detached(link.uri);
                }

                execute!(io::stdout(), SetPointerShape(PointerShape::Default))
                    .expect("Could not set pointer shape");

                /* Since the mouse position is saved but this loop runs on other triggers
                 * such as key press, don't allow the mouse to accidentely click on other
                 * links when the viewer is scrolled down by key presses */
                current_mouse.kind = MouseEventKind::Moved;
            }
        } else {
            execute!(io::stdout(), SetPointerShape(PointerShape::Default))
                .expect("Could not set pointer shape");
        }

        let gr = RECEIVER_GR.get().unwrap().lock().unwrap();
        /* Responses that arrived late belong to earlier placements */
        while gr.try_recv().is_ok() {}

        let displayed = viewer
            .display_pages(&renderer)
            .expect("Could not display pages");
        for _ in displayed {
            /* Never block forever on a missing response from the terminal */
            let Ok(mut res) = gr.recv_timeout(Duration::from_millis(500)) else {
                break;
            };

            /* Only an image that the terminal has dropped (e.g. after the screen
             * was cleared) is worth rendering again; doing so on any other failure
             * would loop forever */
            if res.payload().starts_with("ENOENT") {
                if let Some(id) = res.control().get("i").and_then(|i| i.parse().ok()) {
                    viewer.invalidate_image(id);
                }
            }
        }
    }

    RUNNING.store(false, Ordering::Release);

    /* ========================== Cook the terminal on exit ========================== */
    execute!(io::stdout(), DisableMouseCapturePixels)
        .expect("Could not disable mouse capture");
    execute!(io::stdout(), LeaveAlternateScreen).expect("Could not leave alt mode");
    execute!(io::stdout(), Show).expect("Could not show cursor");
    disable_raw_mode().expect("Could not uncook the terminal");
}

fn handle_mouse_scroll(kind: MouseEventKind, viewer: &mut Viewer) -> bool {
    let config = CONFIG.get().unwrap();

    let inverse_factor = if config.viewer.inverse_scroll {
        1.0
    } else {
        -1.0
    };

    match kind {
        MouseEventKind::ScrollUp => {
            viewer.scroll((0.0f32, inverse_factor * config.viewer.scroll_speed));
            false
        }
        MouseEventKind::ScrollDown => {
            viewer.scroll((0.0f32, -inverse_factor * config.viewer.scroll_speed));
            false
        }
        MouseEventKind::ScrollLeft => {
            viewer.scroll((-config.viewer.scroll_speed, 0.0f32));
            false
        }
        MouseEventKind::ScrollRight => {
            viewer.scroll((config.viewer.scroll_speed, 0.0f32));
            false
        }
        _ => false,
    }
}

fn handle_key(
    key: KeyEvent,
    key_matcher: &mut Keybinds<ConfigAction>,
    viewer: &mut Viewer,
    renderer: &threads::renderer::Renderer,
    throttle_data: &mut LastExecuted,
    toc: &mut Toc,
    follow: &mut Follow,
) -> bool {
    let config = CONFIG.get().unwrap();

    let possible_action = key_matcher.dispatch(KeyInput::from(key));
    if possible_action.is_none() {
        return false;
    }

    let action = possible_action.unwrap();

    /* `true` indicates that the caller should exit *safely* the current process */
    match action {
        ConfigAction::MoveUp => {
            viewer.scroll((0.0f32, -config.viewer.scroll_speed));
            false
        }
        ConfigAction::MoveDown => {
            viewer.scroll((0.0f32, config.viewer.scroll_speed));
            false
        }
        ConfigAction::MoveLeft => {
            viewer.scroll((-config.viewer.scroll_speed, 0.0f32));
            false
        }
        ConfigAction::MoveRight => {
            viewer.scroll((config.viewer.scroll_speed, 0.0f32));
            false
        }
        ConfigAction::PrevPage => {
            let current_page = viewer.page_first();
            if current_page > 0 {
                let _ = viewer.jump(current_page - 1);
            }
            false
        }
        ConfigAction::NextPage => {
            let current_page = viewer.page_first();
            let last_page = viewer.pages() - 1;
            if current_page < last_page {
                let _ = viewer.jump(current_page + 1);
            }
            false
        }
        ConfigAction::CenterViewer => {
            viewer.center_viewer();
            false
        }
        ConfigAction::JumpFirstPage => {
            let _ = viewer.jump(0);
            false
        }
        ConfigAction::JumpLastPage => {
            let last_page = viewer.pages() - 1;
            let _ = viewer.jump(last_page);
            false
        }
        ConfigAction::ToggleToc => {
            toc.open = true;
            false
        }
        ConfigAction::ToggleFollow => {
            follow.enabled = !follow.enabled;
            if follow.enabled {
                follow.apply(viewer);
            }
            false
        }
        ConfigAction::Quit => true,
        ConfigAction::ToggleAlpha => {
            if throttle_data.alpha.elapsed().unwrap() < Duration::from_millis(500) {
                return false;
            }

            throttle_data.alpha = SystemTime::now();

            renderer
                .send_and_confirm_action(threads::renderer::RendererAction::ToggleAlpha)
                .expect("Could not send action to renderer");
            viewer.invalidate_registry();
            false
        }
        ConfigAction::ToggleInverse => {
            if throttle_data.inverse.elapsed().unwrap() < Duration::from_millis(500) {
                return false;
            }

            throttle_data.inverse = SystemTime::now();
            renderer
                .send_and_confirm_action(threads::renderer::RendererAction::ToggleInverse)
                .expect("Could not send action to renderer");
            viewer.invalidate_registry();
            false
        }
        ConfigAction::ZoomIn => {
            viewer.scale(config.viewer.scale_amount);
            false
        }
        ConfigAction::ZoomOut => {
            viewer.scale(-config.viewer.scale_amount);
            false
        }
    }
}
