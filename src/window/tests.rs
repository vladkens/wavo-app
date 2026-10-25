// Copyright (c) vladkens | MIT License | https://github.com/vladkens/wavo-app

//! The settings window in nimbo's harness, headless, with a fake shell over a Hugging Face cache
//! and a history in a temp dir: every page renders in both looks and fits its window or scrolls
//! in it, the toolbar switches pages, controls show their hover and the pointing hand, ⓘ shows
//! its tooltip, the recorder records a hotkey (pausing the current one meanwhile), selects and
//! switches edit the settings, launch at login flips at once and waits for the system (turning
//! back when it fails), models are found by search and language, one downloads and another is
//! used, a downloaded model is deleted after asking in the modal dialog, and the history selects
//! a row without opening it, shows its whole text in a tooltip, copies, transcribes again and
//! deletes after asking. The UI rules: a model row keeps one height in every state, a control
//! keeps its width whatever its row says.
//!
//! `WAVO_SHOTS=<dir>` (`make shots`: `target/shots`) also writes each page's shot there.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use nimbo::permissions::Access as Grant;
use nimbo::{
  Appearance, Code, Cursor, Event, Harness, Hotkey, Id, Modifiers, NamedKey, Point, Rect, Size,
};
use wavo::hub;

use super::general::{
  ACTIVATION, DOWNLOAD_MODEL, HOTKEY, HUD, KEEP, LANG, LOGIN, MIC, MODEL, MODEL_FOLDER,
  OPEN_ACCESSIBILITY, OPEN_MICROPHONE, PASTE, RECORDINGS_FOLDER, RESET, RETENTION, TRAY_ONLY,
  UNLOAD, row,
};
use super::history::{COPY, DELETE, PLAY, RETRANSCRIBE, ROW, SEARCH, TEXT, UNDO};
use super::models::{self as page, CANCEL, DOWNLOAD, INFO, USE};
use super::*;
use crate::audio::devices::Choice;
use crate::history::db::NewEntry;
use crate::history::{History, now_ms};
use crate::session::controller::{EngineStatus, Phase};
use crate::settings::Activation;
use crate::testing::TempDir;
use crate::transfer::Report;
use crate::transfer::archive::Exported;
use crate::transfer::handy::Summary;

#[derive(Default)]
struct Log {
  /// The session's settings: the edits applied in order.
  settings: Settings,
  edits: usize,
  paused: Vec<bool>,
  /// The downloads asked for, by name; their states as the test sets them.
  downloads: Vec<&'static str>,
  states: HashMap<&'static str, Download>,
  cancelled: Vec<String>,
  retranscribed: Vec<i64>,
  undone: Vec<i64>,
  deleted: Vec<i64>,
  urls: Vec<String>,
  folders: Vec<PathBuf>,
  /// The cache models are deleted from, and how each delete went, for the test to hand to
  /// [Prefs::model_deleted].
  hub: Option<Hub>,
  deleted_models: Vec<(&'static ModelInfo, Result<u64, String>)>,
  /// How often the window asked the session to look at the models again.
  checks: usize,
  /// Launch at login asked for, in order; the test hands the answers to [Prefs::autostart_set].
  autostarts: Vec<bool>,
  /// The recordings' size, added up unless set.
  recordings_size: Option<u64>,
  /// What the system allows, as the test sets it: Accessibility only missing unless set.
  access: Option<Access>,
  /// The permissions Allow or Open Settings was clicked for.
  allowed: Vec<Pane>,
  /// Handy's history as the shell finds it; the import asked for, with what tells it how it goes;
  /// the archives exported.
  handy: Option<Summary>,
  import: Option<Box<dyn Fn(data::Import) + Send>>,
  exports: Vec<PathBuf>,
}

#[derive(Clone, Default)]
struct Fake(Rc<RefCell<Log>>);

impl Fake {
  fn log(&self) -> std::cell::RefMut<'_, Log> {
    self.0.borrow_mut()
  }
}

impl Shell for Fake {
  fn edit(&self, edit: Box<dyn FnOnce(&mut Settings) + Send>) {
    let mut log = self.log();
    edit(&mut log.settings);
    log.edits += 1;
  }

  fn pause_hotkey(&self, paused: bool) {
    self.log().paused.push(paused);
  }

  fn devices(&self) -> Vec<devices::Info> {
    let info = |name: &str, default| devices::Info {
      id: Some(name.to_lowercase()),
      name: name.into(),
      default,
      channels: Some(1),
      rate: Some(48_000),
      format: None,
    };
    let names = [("MacBook Pro Microphone", true), ("USB Mic", false), ("RØDE PodMic USB", false)];
    names.into_iter().map(|(name, default)| info(name, default)).collect()
  }

  /// It runs 42% through until the test says otherwise.
  fn download(&self, model: &'static ModelInfo) {
    let mut log = self.log();
    log.downloads.push(model.name);
    let running = Download::Running { done: model.size * 42 / 100, total: model.size, rate: None };
    log.states.insert(model.name, running);
  }

  fn cancel_download(&self, name: &str) {
    let mut log = self.log();
    log.states.retain(|n, _| *n != name);
    log.cancelled.push(name.to_owned());
  }

  fn download_state(&self, name: &str) -> Option<Download> {
    self.log().states.get(name).cloned()
  }

  /// What the shells do on their thread, here at once; the file goes, not through
  /// `Hub::remove` ([page::delete]), which also looks at the Neural Engine's cache in the
  /// person's home.
  fn delete_model(&self, model: &'static ModelInfo) {
    let mut log = self.log();
    let hub = log.hub.clone().expect("a cache to delete from");
    let file = hub.find(model.name).expect("downloaded");
    let result = std::fs::remove_file(&file).map(|()| model.size).map_err(|e| e.to_string());
    if result.is_ok() {
      page::forget(&mut log.settings, &hub, model);
    }

    log.deleted_models.push((model, result));
  }

  fn check_model(&self) {
    self.log().checks += 1;
  }

  fn retranscribe(&self, id: i64) {
    self.log().retranscribed.push(id);
  }

  fn undo_transcribe(&self, id: i64) {
    self.log().undone.push(id);
  }

  fn delete_entry(&self, id: i64) {
    self.log().deleted.push(id);
  }

  fn autostart(&self) -> Result<Autostart, String> {
    Ok(Autostart::NotRegistered)
  }

  fn set_autostart(&self, on: bool) {
    self.log().autostarts.push(on);
  }

  fn access(&self) -> Access {
    let missing = Access { microphone: Grant::Granted, ..Access::default() };
    self.log().access.unwrap_or(missing)
  }

  /// At once, on the window's thread.
  fn folder_size(&self, dir: PathBuf, done: Box<dyn FnOnce(u64) + Send>) {
    let size = self.log().recordings_size;
    done(size.unwrap_or_else(|| folder_size(&dir)));
  }

  fn allow(&self, pane: Pane) {
    self.log().allowed.push(pane);
  }

  fn open_url(&self, url: &str) {
    self.log().urls.push(url.to_owned());
  }

  fn reveal(&self, dir: &Path) {
    self.log().folders.push(dir.to_owned());
  }

  fn handy(&self, done: Box<dyn FnOnce(Option<Summary>) + Send>) {
    let found = self.log().handy.clone();
    done(found);
  }

  fn import_handy(&self, _: PathBuf, tell: Box<dyn Fn(data::Import) + Send>) {
    self.log().import = Some(tell);
  }

  /// Done at once, written nowhere.
  fn export(&self, file: PathBuf, done: Box<dyn FnOnce(Result<Exported, String>) + Send>) {
    self.log().exports.push(file);
    done(Ok(Exported { entries: 3, recordings: 2, bytes: 1_200_000 }));
  }
}

fn status(state: Phase) -> Status {
  Status {
    state,
    activation: Activation::HoldOrToggle,
    entry: None,
    recording: None,
    engine: EngineStatus::default(),
    last: None,
    retranscribing: Vec::new(),
    error: None,
    warnings: Vec::new(),
    no_model: None,
  }
}

/// A home in a temp dir with a Hugging Face cache in it, `hub/`, that has `models`.
struct Home {
  dir: TempDir,
  hub: Hub,
}

impl Home {
  fn new(name: &str, models: &[&str]) -> Self {
    let dir = TempDir::new(name);
    let hub = Hub::at(dir.path().join("hub"));
    for m in models {
      crate::testing::download(&hub, m);
    }

    Self { dir, hub }
  }

  fn path(&self) -> &Path {
    self.dir.path()
  }
}

/// Dictations in `home`'s history, `(text, minutes ago, seconds)`, the oldest first, with a
/// recording each when `audio`; one without text was tried by whisper-tiny. Their ids.
fn history_of(home: &Path, items: &[(&str, i64, f32)], audio: bool) -> Vec<i64> {
  let h = History::open(home).unwrap();
  let now = now_ms();
  (items.iter())
    .map(|&(text, ago, secs)| {
      let (name, path) = h.recordings.new_file(now - ago * 60_000);
      if audio {
        crate::testing::write_wav(&path, &[0.0; 1_600]);
      }

      let (model, tried) = ("parakeet-tdt-0.6b-v3-Q8_0", "whisper-tiny-Q8_0");
      let entry = NewEntry {
        created_at: now - ago * 60_000,
        duration_ms: (secs * 1000.0) as i64,
        audio_path: audio.then_some(name.as_str()),
        text,
        model: (!text.is_empty()).then_some(model),
        tried_model: text.is_empty().then_some(tried),
        ..NewEntry::default()
      };
      h.db.insert_entry(&entry).unwrap()
    })
    .collect()
}

/// A few dictations, newest last; their ids.
fn dictations(home: &Path) -> Vec<i64> {
  history_of(
    home,
    &[
      ("Let's move the design review to Thursday afternoon.", 3, 2.4),
      ("Remind me to send the invoice to Anna before Friday.", 2, 3.3),
      ("The build is green again, the flaky test was a race in the pool.", 1, 4.2),
    ],
    false,
  )
}

/// Today's and yesterday's dictations, more than the window holds, with recordings: one without
/// text, and the newest transcribed again (it can be undone).
fn many_dictations(home: &Path) -> Vec<i64> {
  let ids = history_of(
    home,
    &[
      ("Draft a reply to Anna: the build is green, the release goes out on Monday.", 1500, 8.4),
      ("Can we keep the help text short enough to fit in one tooltip?", 1440, 5.1),
      ("Купить кофе, молоко и батарейки для микрофона.", 1410, 3.8),
      ("Ship the CPU renderer first; Metal stays opt-in until we measure it.", 18, 7.0),
      ("Синхронизация работает нормально, если подносить телефон к QR-экрану.", 17, 29.7),
      ("Remind me to check the parakeet download on the Linux box tonight.", 7, 4.2),
      ("В списке слов кнопка должна оставаться на месте, когда я листаю.", 5, 15.6),
      (
        "Let’s move the settings to top tabs and drop the sidebar for now. Models and history \
         become plates of rows, and every row keeps its play and copy buttons, so I can check \
         what it heard.",
        2,
        11.9,
      ),
      ("Отдельно я хотел бы чтобы ты импортировал эту штуку отдельным экшеном", 1, 6.5),
      ("", 4, 2.1),
    ],
    true,
  );
  let again = "Отдельно я хотел бы, чтобы ты импортировал эту штуку отдельным экшеном.";
  let h = History::open(home).unwrap();
  h.db.update_text(ids[8], again, "whisper-large-v3-turbo-Q8_0", None).unwrap();
  ids
}

/// The two models most tests have downloaded: the first two of wavo's list.
const TWO: [&str; 2] = ["parakeet-v3", "parakeet-v2"];

fn harness(fake: &Fake, settings: Settings, home: &Home) -> Harness<Prefs> {
  fake.log().settings = settings.clone();
  fake.log().hub = Some(home.hub.clone());
  let mut prefs =
    Prefs::new(Box::new(fake.clone()), settings.clone(), home.hub.clone(), home.path());
  prefs.update(&settings, &status(Phase::Idle), 1);
  let mut h = Harness::new(prefs, config(Page::General).size, 2.0);
  h.event(Event::Appearance(Appearance::LIGHT));
  h.frame();
  h.frame();
  h
}

/// Clicks `id` and lets the frames after it settle (a select's list opens on the next one).
fn click(h: &mut Harness<Prefs>, id: Id) {
  assert!(h.rect(id).is_some(), "{id:?} is not shown");
  h.click_id(id);
  h.frame();
  h.frame();
}

fn tab(h: &mut Harness<Prefs>, page: Page) {
  let i = PAGES.iter().position(|p| p.0 == page).unwrap();
  click(h, Id::of(TABS).key(i));
  h.frame();
  assert_eq!(h.app().page, page);
}

fn center(r: Rect) -> Point {
  Point::new(r.x + r.w / 2.0, r.y + r.h / 2.0)
}

/// A trackpad gesture of one step at (x, y); a negative `dy` moves the content up.
fn scroll(h: &mut Harness<Prefs>, x: f32, y: f32, dy: f32) {
  use nimbo::app::Phase as Gesture;

  let (pos, momentum, mods) = (Point::new(x, y), Gesture::None, Modifiers::empty());
  for (phase, delta) in
    [(Gesture::Began, Point::new(0.0, 0.0)), (Gesture::Changed, Point::new(0.0, dy))]
  {
    h.event(Event::Scroll { pos, delta, precise: true, phase, momentum, mods });
  }
}

fn page_scroll(h: &Harness<Prefs>, page: Page) -> nimbo::Scroll {
  h.rt.ui.scroll(Id::of(("page", page as u8))).expect("the page's scroll area")
}

/// The cursor over `id` and how many pixels of its top half changed from the pointer away to
/// the pointer resting on it (fades done).
fn hover(h: &mut Harness<Prefs>, id: Id) -> (Cursor, usize) {
  let r = h.rect(id).unwrap_or_else(|| panic!("no element {id:?}"));
  h.leave();
  h.wait(Duration::from_millis(400));
  let away = h.render();
  let c = center(r);
  h.move_to(c.x, c.y);
  h.wait(Duration::from_millis(400));
  let over = h.render();
  let mut n = 0;
  for y in 0..r.h as usize {
    for x in 0..(r.w * 2.0) as usize {
      let (px, py) = (r.x + x as f32 / 2.0, r.y + y as f32 / 2.0);
      n += (away.pixel(px, py) != over.pixel(px, py)) as usize;
    }
  }

  (h.rt.cursor(), n)
}

fn hovers_on(h: &mut Harness<Prefs>, ids: &[Id]) {
  for &id in ids {
    let (cursor, n) = hover(h, id);
    assert!(cursor == Cursor::Pointer && n > 0, "{id:?}: {cursor:?}, {n} pixels");
  }
}

/// Each page at its window size: General, Advanced and About as high as their content (it fits,
/// no blank under it), Models and History in a window [SCROLLING] high, scrolling to their last
/// row; every control inside the window.
#[test]
fn pages_fit_or_scroll_and_the_toolbar_switches_them() {
  let home = Home::new("window-fit", &TWO);
  let ids = many_dictations(home.path());
  let older = (2..8).map(|d| ("An older dictation.", d * 1440, 2.0)).collect::<Vec<_>>();
  let oldest = *history_of(home.path(), &older, false).last().unwrap();
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  let general = [OPEN_ACCESSIBILITY, OPEN_MICROPHONE, HOTKEY, RESET, ACTIVATION, MIC, MODEL, LANG]
    .into_iter()
    .chain([PASTE, KEEP, HUD, LOGIN, TRAY_ONLY])
    .map(Id::of)
    .collect();
  let controls: [Vec<Id>; 5] = [
    general,
    vec![Id::of(page::SEARCH), Id::of(page::LANG), Id::of((INFO, TWO[0])), Id::of((USE, TWO[1]))],
    vec![Id::of(SEARCH), Id::of((COPY, ids[8])), Id::of((PLAY, ids[8]))],
    [UNLOAD, RETENTION, MODEL_FOLDER, RECORDINGS_FOLDER].map(Id::of).to_vec(),
    vec![Id::of((about::LINK, "source")), Id::of((about::LINK, "models"))],
  ];
  let last_model = hub::MODELS.last().unwrap().name;
  let last = [Id::of((DOWNLOAD, last_model)), Id::of((COPY, oldest))];
  for (i, (page, _, name)) in PAGES.iter().enumerate() {
    tab(&mut h, *page);
    let size = h.rt.size;
    let inside = |r: Rect| r.x >= 16.0 && r.x + r.w <= size.w - 16.0 && r.y >= 0.0;
    for id in &controls[i] {
      let r = h.rect(*id).unwrap_or_else(|| panic!("{name}: {id:?} is not shown"));
      assert!(inside(r) && r.y + r.h <= size.h, "{name}: {id:?} at {r:?} in {size:?}");
    }

    let sc = page_scroll(&h, *page);
    if !page.scrolls() {
      assert!(size.h < SCROLLING + 200.0 && size.w == WIDTH, "{name}: {size:?}");
      let blank = sc.viewport.h - sc.content.h;
      assert!((-0.5..=1.0).contains(&blank), "{name}: fits, no blank: {sc:?}");
      continue;
    }

    assert_eq!(size, Size::new(WIDTH, SCROLLING), "{name}");
    assert!(sc.content.h > sc.viewport.h, "{name}: {sc:?}");
    let c = center(sc.viewport);
    for _ in 0..40 {
      scroll(&mut h, c.x, c.y, -200.0);
      h.frame();
    }

    let id = last[i - 1];
    let r = h.rect(id).unwrap_or_else(|| panic!("{name}: {id:?} is not shown"));
    assert!(inside(r) && r.y + r.h <= size.h, "{name}: {id:?} at {r:?} scrolled down");
  }

  // Tab focuses the toolbar first; the arrows step.
  h.leave();
  h.key(NamedKey::Tab, Modifiers::empty());
  assert_eq!(h.focused(), Some(Id::of(TABS)));
  h.key(NamedKey::ArrowLeft, Modifiers::empty());
  h.frame();
  h.frame();
  assert_eq!(h.app().page, Page::Advanced);
  assert!(!config(Page::General).resizable);
}

/// Hover and the pointing hand on every kind of control, on each page; none on a disabled one;
/// an ⓘ's tooltip after a rest.
#[test]
fn controls_show_hover_and_tooltips() {
  let home = Home::new("window-hover", &TWO);
  let ids = dictations(home.path());
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  let info = |name: &str| row(name).key("info");
  let mut general = vec![Id::of(OPEN_ACCESSIBILITY), Id::of(OPEN_MICROPHONE), Id::of(HOTKEY)];
  general.extend([info(ACTIVATION), Id::of(MIC), Id::of(MODEL)]);
  general.extend([Id::of(LANG), Id::of(PASTE), Id::of(HUD)]);
  general.extend([Id::of(LOGIN), Id::of(TABS).key(1)]);
  hovers_on(&mut h, &general);
  assert_eq!(hover(&mut h, Id::of(RESET)), (Cursor::Arrow, 0), "reset is disabled at the default");

  // The tooltip shows under the ⓘ after a rest.
  let tip = info(ACTIVATION).key("tip");
  h.leave();
  h.wait(Duration::from_millis(1500));
  let c = center(h.rect(info(ACTIVATION)).unwrap());
  h.move_to(c.x, c.y);
  h.wait(Duration::from_millis(100));
  assert!(h.rect(tip).is_none(), "not yet");
  h.wait(Duration::from_millis(300));
  assert!(h.rect(tip).is_some(), "after the delay");

  // Keeping the clipboard means nothing without pasting.
  click(&mut h, Id::of(PASTE));
  assert_eq!(hover(&mut h, Id::of(KEEP)), (Cursor::Arrow, 0), "the row is disabled");

  tab(&mut h, Page::Models);
  let remote = hub::MODELS[2].name;
  let models = [Id::of(page::LANG), Id::of((INFO, TWO[0])), Id::of((USE, TWO[1]))];
  hovers_on(&mut h, &models);
  hovers_on(&mut h, &[Id::of((page::DELETE, TWO[1])), Id::of((DOWNLOAD, remote))]);

  tab(&mut h, Page::History);
  hovers_on(&mut h, &[Id::of((COPY, ids[2])), Id::of((DELETE, ids[2]))]);
  // No recording to play or to transcribe again in this history.
  assert_eq!(hover(&mut h, Id::of((PLAY, ids[2]))), (Cursor::Arrow, 0));
  assert_eq!(hover(&mut h, Id::of((RETRANSCRIBE, ids[2]))), (Cursor::Arrow, 0));
  // The search field's ⊗ once it has text.
  click(&mut h, Id::of(SEARCH));
  h.type_text("build");
  h.frame();
  hovers_on(&mut h, &[Id::of(SEARCH).key("clear")]);

  tab(&mut h, Page::Advanced);
  let advanced = [info(UNLOAD), Id::of(RETENTION), Id::of(MODEL_FOLDER), Id::of(RECORDINGS_FOLDER)];
  hovers_on(&mut h, &advanced);
  click(&mut h, Id::of(MODEL_FOLDER));
  click(&mut h, Id::of(RECORDINGS_FOLDER));
  let recordings = home.path().join(crate::history::recordings::DIR);
  assert_eq!(fake.log().folders, [home.hub.root().to_owned(), recordings]);

  tab(&mut h, Page::About);
  hovers_on(&mut h, &[Id::of((about::LINK, "source")), Id::of((about::LINK, "ggml"))]);
  click(&mut h, Id::of((about::LINK, "source")));
  click(&mut h, Id::of((about::LINK, "handy")));
  let urls = ["https://github.com/vladkens/wavo-app", "https://github.com/cjpais/Handy"];
  assert_eq!(fake.log().urls, urls);
}

#[test]
fn the_recorder_records_a_hotkey_with_the_current_one_paused() {
  let home = Home::new("window-hotkey", &TWO);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  click(&mut h, Id::of(HOTKEY));
  assert_eq!(h.focused(), Some(Id::of(HOTKEY)));
  assert_eq!(fake.log().paused, [true]);
  h.key('k', Modifiers::CONTROL | Modifiers::ALT);
  h.frame();
  h.frame();
  let want = "ctrl+option+k";
  assert_eq!(fake.log().settings.hotkey, want);
  assert_eq!(h.app().settings.hotkey, want);
  assert_eq!(fake.log().paused, [true, false], "the hotkey is back");

  // Escape cancels; the reset button restores the default.
  click(&mut h, Id::of(HOTKEY));
  h.key(NamedKey::Escape, Modifiers::empty());
  h.frame();
  assert_eq!(fake.log().settings.hotkey, want);
  assert_eq!(fake.log().paused, [true, false, true, false]);
  click(&mut h, Id::of(RESET));
  assert_eq!(fake.log().settings.hotkey, Settings::default().hotkey);

  // Leaving the page while recording brings the hotkey back.
  click(&mut h, Id::of(HOTKEY));
  tab(&mut h, Page::About);
  assert_eq!(fake.log().paused, [true, false, true, false, true, false]);
}

/// Left Option alone and fn alone: the recorder takes the key pressed and released by itself
/// (synthetic [Event::Modifier]s, as the window hands them over), the session saves it, it loads
/// back from settings.json, and nimbo's hotkey parses it to the key the tap matches.
#[test]
fn the_recorder_saves_a_lone_modifier_the_hotkey_matches() {
  let home = Home::new("window-lone", &TWO);
  let file = home.path().join(crate::settings::FILE);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  let keys = [(Code::AltLeft, "left_option"), (Code::Fn, "fn"), (Code::AltRight, "right_option")];
  for (n, (key, saved)) in keys.into_iter().enumerate() {
    click(&mut h, Id::of(HOTKEY));
    assert_eq!(fake.log().paused.len(), 2 * n + 1, "paused while recording");
    h.event(Event::Modifier { key, down: true });
    assert_eq!(h.focused(), Some(Id::of(HOTKEY)), "{saved}: still recording while it is down");
    h.event(Event::Modifier { key, down: false });
    h.frame();
    h.frame();
    assert_eq!(fake.log().settings.hotkey, saved);
    let back = (h.focused(), fake.log().paused.last().copied());
    assert_eq!(back, (None, Some(false)), "{saved}: the hotkey is back");

    fake.log().settings.save(&file).unwrap();
    let (loaded, warning) = Settings::load(&file);
    assert_eq!((loaded.hotkey.as_str(), warning), (saved, None));
    assert_eq!(Hotkey::parse(&loaded.hotkey).unwrap(), Hotkey::Modifier(key), "{saved}");
  }

  // fn says what macOS does with it too.
  click(&mut h, Id::of(TABS).key(0));
  fake.log().settings.hotkey = "fn".into();
  let settings = fake.log().settings.clone();
  h.app_mut().update(&settings, &status(Phase::Idle), 1);
  h.rt.waker().wake();
  h.frame();
}

#[test]
fn selects_and_switches_edit_the_settings() {
  let home = Home::new("window-selects", &TWO);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  click(&mut h, Id::of(ACTIVATION));
  click(&mut h, Id::of(ACTIVATION).key(("item", 2)));
  assert_eq!(fake.log().settings.activation, Activation::Toggle);
  assert_eq!(h.app().settings.activation, Activation::Toggle);

  click(&mut h, Id::of(MIC));
  click(&mut h, Id::of(MIC).key(("item", 2)));
  let usb = Choice { id: Some("usb mic".into()), name: "USB Mic".into() };
  assert_eq!(fake.log().settings.microphone, Some(usb));

  click(&mut h, Id::of(KEEP));
  assert!(fake.log().settings.keep_in_clipboard);
  click(&mut h, Id::of(PASTE));
  assert!(!fake.log().settings.auto_paste);
  click(&mut h, Id::of(KEEP));
  assert!(fake.log().settings.keep_in_clipboard, "disabled while pasting is off");

  click(&mut h, Id::of(TRAY_ONLY));
  assert!(fake.log().settings.start_in_tray);
  // Launch at login goes to the system, not through the settings' edits.
  click(&mut h, Id::of(LOGIN));
  assert_eq!(fake.log().autostarts, [true]);

  tab(&mut h, Page::Advanced);
  click(&mut h, Id::of(RETENTION));
  click(&mut h, Id::of(RETENTION).key(("item", 1)));
  assert_eq!(fake.log().settings.history_retention, crate::history::Retention::Count(100));
  assert_eq!(fake.log().edits, 6);
}

/// General's Model pop-up: the downloaded models, the one in use checked, then "Manage models…".
/// A model picked is used as Models' Use does, with a note when it is meant for one language;
/// the last item goes to the Models page.
#[test]
fn the_general_model_pop_up_switches_models() {
  let home = Home::new("window-model-pick", &TWO);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  assert_eq!(h.app().model_note(), None, "parakeet-v3 knows 25 languages");
  let before = h.rt.size.h;
  click(&mut h, Id::of(MODEL));
  for i in 0..3 {
    assert!(h.rect(Id::of(MODEL).key(("item", i))).is_some(), "item {i}");
  }

  assert!(h.rect(Id::of(MODEL).key(("item", 3))).is_none(), "two models and Manage");
  click(&mut h, Id::of(MODEL).key(("item", 1)));
  assert_eq!(fake.log().settings.model.as_deref(), Some(TWO[1]));
  let note = h.app().model_note();
  assert_eq!(note.as_deref(), Some("parakeet-tdt-0.6b-v2 understands English only."));
  // The caption makes the row taller: the window grows to it.
  h.frame();
  h.frame();
  assert!(h.rt.size.h > before, "{before} → {:?}", h.rt.size);
  let sc = page_scroll(&h, Page::General);
  assert!(sc.content.h <= sc.viewport.h + 0.5, "{sc:?}");

  // The current one again changes nothing; the last item goes to Models.
  let edits = fake.log().edits;
  click(&mut h, Id::of(MODEL));
  click(&mut h, Id::of(MODEL).key(("item", 1)));
  assert_eq!(fake.log().edits, edits);
  click(&mut h, Id::of(MODEL));
  click(&mut h, Id::of(MODEL).key(("item", 2)));
  assert_eq!(h.app().page, Page::Models);
  assert!(h.rect(Id::of((USE, TWO[0]))).is_some(), "on the Models page");

  // The language pop-up lists what the model in use is meant for: English alone.
  tab(&mut h, Page::General);
  click(&mut h, Id::of(LANG));
  assert!(h.rect(Id::of(LANG).key(("item", 1))).is_some());
  assert!(h.rect(Id::of(LANG).key(("item", 2))).is_none(), "auto and English");
  click(&mut h, Id::of(LANG).key(("item", 1)));
  assert_eq!(fake.log().settings.language, "en");
}

/// Opens pop-up `id`, types `c` and picks the highlighted item with Return.
fn type_select(h: &mut Harness<Prefs>, id: Id, c: char) {
  h.leave();
  click(h, id);
  assert!(h.rect(id.key("menu")).is_some(), "{id:?} open");
  h.key(c, Modifiers::empty());
  h.frame();
  h.key(NamedKey::Enter, Modifiers::empty());
  h.frame();
  h.frame();
  assert!(h.rect(id.key("menu")).is_none(), "{id:?} closed by the pick");
}

/// Typing "r" in the open Language, Microphone and Unload pop-ups highlights the first item that
/// starts with R, which Return picks.
#[test]
fn pop_ups_select_by_typing() {
  let home = Home::new("window-type-select", &TWO);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  type_select(&mut h, Id::of(LANG), 'r');
  assert_eq!(fake.log().settings.language, "ro", "Romanian, the first in R of parakeet-v3's");
  type_select(&mut h, Id::of(MIC), 'r');
  let rode = Choice { id: Some("røde podmic usb".into()), name: "RØDE PodMic USB".into() };
  assert_eq!(fake.log().settings.microphone, Some(rode));
  tab(&mut h, Page::Advanced);
  type_select(&mut h, Id::of(UNLOAD), 'r');
  assert_eq!(fake.log().settings.unload_after_minutes, Some(0), "Right after each dictation");
}

/// The switch flips at once while the system is asked off the main thread; the answer keeps it
/// or turns it back with the reason under the row. A click meanwhile goes next.
#[test]
fn launch_at_login_flips_at_once_and_turns_back_on_failure() {
  let home = Home::new("window-login", &TWO);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  click(&mut h, Id::of(LOGIN));
  assert_eq!(fake.log().autostarts, [true], "asked of the shell");
  assert!(h.app().login.shown(false), "on before the answer");
  assert_eq!(fake.log().edits, 0);

  h.app_mut().autostart_set(true, Ok(Autostart::Enabled));
  h.frame();
  assert_eq!(h.app().login.state, Ok(Autostart::Enabled));
  assert!(h.app().settings.autostart);

  // Refused: it goes back and says why under the row, the switch as wide as before.
  let width = h.rect(Id::of(LOGIN)).unwrap().w;
  click(&mut h, Id::of(LOGIN));
  assert_eq!(fake.log().autostarts, [true, false]);
  h.app_mut().autostart_set(false, Err("unregister: Operation not permitted (code 1)".into()));
  h.rt.waker().wake();
  h.wait(Duration::from_secs(1));
  assert!(h.app().login.shown(false), "on again");
  let caption = h.text(row(LOGIN).key("caption")).map(str::to_owned);
  let want = "Couldn't turn it off: unregister: Operation not permitted (code 1)";
  assert_eq!(caption.as_deref(), Some(want));
  assert_eq!(h.rect(Id::of(LOGIN)).unwrap().w, width);

  // A click while one is on its way goes once it ends; the error is gone.
  click(&mut h, Id::of(LOGIN));
  click(&mut h, Id::of(LOGIN));
  assert_eq!(fake.log().autostarts, [true, false, false], "one at a time");
  assert_eq!(h.app().login.error, None);
  h.app_mut().autostart_set(false, Ok(Autostart::NotRegistered));
  assert_eq!(fake.log().autostarts, [true, false, false, true], "the one asked meanwhile");
  h.app_mut().autostart_set(true, Ok(Autostart::RequiresApproval));
  assert!(h.app().login.shown(false) && h.app().login.busy.is_none());
}

/// The names of wavo's models with a row on the page, in its order.
fn shown(h: &Harness<Prefs>) -> Vec<&'static str> {
  (hub::MODELS.iter())
    .filter(|m| h.rect(Id::of((page::ROW, m.name))).is_some())
    .map(|m| m.name)
    .collect()
}

#[test]
fn models_are_found_downloaded_and_used() {
  let home = Home::new("window-models", &TWO);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  tab(&mut h, Page::Models);
  let remote = hub::MODELS[2].name;
  click(&mut h, Id::of((DOWNLOAD, remote)));
  assert_eq!(fake.log().downloads, [remote]);
  assert!(h.rect(Id::of((DOWNLOAD, remote))).is_none(), "its progress instead");
  click(&mut h, Id::of((CANCEL, remote)));
  assert_eq!(fake.log().cancelled, [remote]);
  assert!(h.rect(Id::of((DOWNLOAD, remote))).is_some());

  // The first found is in use without a choice, with no Use; the second can be. Both keep their
  // places when the second is used.
  let places = |h: &Harness<Prefs>| TWO.map(|n| h.rect(Id::of((page::ROW, n))).unwrap().y);
  let before = places(&h);
  assert!(before[0] < before[1], "{before:?}");
  assert!(h.rect(Id::of((USE, TWO[0]))).is_none());
  assert!(h.rect(Id::of((page::DELETE, TWO[0]))).is_some(), "in use: its bin");
  click(&mut h, Id::of((USE, TWO[1])));
  assert_eq!(fake.log().settings.model.as_deref(), Some(TWO[1]));
  assert!(h.rect(Id::of((USE, TWO[1]))).is_none(), "in use now");
  assert!(h.rect(Id::of((USE, TWO[0]))).is_some());
  assert_eq!(places(&h), before, "no row moved");

  // ⓘ opens the model's card.
  click(&mut h, Id::of((INFO, TWO[0])));
  assert_eq!(fake.log().urls, [hub::lookup(TWO[0]).unwrap().card]);

  // The search finds by name; the model in use stays shown, in its place among the downloaded.
  let all = shown(&h).len();
  assert_eq!(all, hub::MODELS.len());
  click(&mut h, Id::of(page::SEARCH));
  h.type_text("moonshine");
  h.frame();
  h.frame();
  assert_eq!(shown(&h), [TWO[1], "moonshine-tiny", "moonshine-base"]);
  click(&mut h, Id::of(page::SEARCH).key("clear"));
  click(&mut h, Id::of(page::SEARCH));
  h.type_text("Russian");
  h.frame();
  h.frame();
  let ru = shown(&h);
  for name in ["gigaam-v3", "parakeet-v3", "whisper-turbo", "whisper-tiny"] {
    assert!(ru.contains(&name), "{name}: {ru:?}");
  }

  assert!(!ru.contains(&"moonshine-base") && !ru.contains(&"whisper-tiny.en"), "{ru:?}");
  click(&mut h, Id::of(page::SEARCH).key("clear"));
  assert_eq!(shown(&h).len(), all);

  // The language pop-up keeps the models meant for it, and the one in use.
  let hebrew = page::LANGUAGES.iter().position(|l| l.1 == "he").unwrap();
  h.app_mut().models.lang = hebrew;
  h.rt.waker().wake();
  h.frame();
  let want: Vec<&str> = (hub::MODELS.iter())
    .filter(|m| m.name == TWO[1] || m.langs.contains(&"he"))
    .map(|m| m.name)
    .collect();
  assert_eq!(shown(&h), want);
  let he = shown(&h);
  assert!(he.contains(&"parakeet-he") && he.contains(&TWO[1]) && !he.contains(&"gigaam-v3"));
}

/// A downloaded model, deleted from the cache after asking in the modal dialog (with the size
/// it frees; Cancel the default): the file goes, it is downloadable again, and a second try
/// says why not.
#[test]
fn a_downloaded_model_is_deleted_after_asking() {
  let home = Home::new("window-delete", &["parakeet-v3", "whisper-tiny"]);
  let fake = Fake::default();
  let settings = Settings { model: Some("parakeet-v3".into()), ..Settings::default() };
  let mut h = harness(&fake, settings, &home);
  tab(&mut h, Page::Models);
  let tiny = hub::lookup("whisper-tiny").unwrap();
  assert!(h.rect(Id::of((USE, "parakeet-v3"))).is_none(), "in use");
  assert!(h.rect(Id::of((USE, tiny.name))).is_some());

  // The bin asks in the dialog; Escape, Return and Cancel keep the model.
  let (cancel, action, card) =
    (Id::of(CONFIRM).key("cancel"), Id::of(CONFIRM).key("action"), Id::of(CONFIRM).key("card"));
  click(&mut h, Id::of((page::DELETE, tiny.name)));
  let (title, message) = h.app().ask.clone().expect("the dialog").question();
  assert_eq!(title, "Delete whisper-tiny?");
  let want = "It frees 46 MB in the Hugging Face cache. You can download it again.";
  assert_eq!(message, want);
  assert!(h.rect(card).is_some(), "the dialog");
  // The backdrop takes the pointer: Use under it does nothing.
  h.click_id(Id::of((USE, tiny.name)));
  h.frame();
  assert_eq!(fake.log().settings.model.as_deref(), Some("parakeet-v3"));
  for close in [NamedKey::Escape, NamedKey::Enter] {
    h.key(close, Modifiers::empty());
    h.frame();
    assert!(h.app().ask.is_none() && h.rect(card).is_none(), "{close:?}");
    click(&mut h, Id::of((page::DELETE, tiny.name)));
  }

  h.wait(Duration::from_millis(200));
  click(&mut h, cancel);
  assert!(h.app().ask.is_none() && fake.log().deleted_models.is_empty());

  click(&mut h, Id::of((page::DELETE, tiny.name)));
  h.wait(Duration::from_millis(200));
  click(&mut h, action);
  assert!(h.rect(card).is_none(), "closed");
  let (model, result) = fake.log().deleted_models.pop().expect("deleted");
  assert_eq!(model.name, tiny.name);
  assert!(result.is_ok(), "{result:?}");
  assert_eq!(home.hub.downloaded().len(), 1);
  assert_eq!(fake.log().settings.model.as_deref(), Some("parakeet-v3"), "still in use");
  assert!(h.rect(Id::of((page::DELETE, tiny.name))).is_none(), "deleting");

  // The shell hands the result back: the model is downloadable again.
  h.app_mut().model_deleted(model, result);
  h.rt.waker().wake();
  h.frame();
  h.frame();
  assert!(h.rect(Id::of((DOWNLOAD, tiny.name))).is_some());

  // A delete that failed says so.
  h.app_mut().model_deleted(model, Err("it is not downloaded".into()));
  let note = h.app().models.note.clone().unwrap_or_default();
  assert!(note.starts_with("Couldn't delete whisper-tiny: "), "{note}");
}

/// The model in use deleted after asking, which says what takes its place: the next one is in
/// use at once; the last one deleted leaves none, said in General, and the session is asked to
/// look again each time.
#[test]
fn the_model_in_use_is_deleted_and_the_next_takes_its_place() {
  let home = Home::new("window-delete-in-use", &["parakeet-v3", "whisper-tiny"]);
  let fake = Fake::default();
  let settings = Settings { model: Some("parakeet-v3".into()), ..Settings::default() };
  let mut h = harness(&fake, settings, &home);
  tab(&mut h, Page::Models);
  let checks = fake.log().checks;
  let delete = |h: &mut Harness<Prefs>, name: &str| {
    click(h, Id::of((page::DELETE, name)));
    let (_, message) = h.app().ask.clone().expect("the dialog").question();
    h.wait(Duration::from_millis(200));
    click(h, Id::of(CONFIRM).key("action"));
    let (model, result) = fake.log().deleted_models.pop().expect("deleted");
    h.app_mut().model_deleted(model, result);
    h.rt.waker().wake();
    h.frame();
    h.frame();
    message
  };

  let message = delete(&mut h, "parakeet-v3");
  let want =
    "It frees 740 MB in the Hugging Face cache. It is in use: whisper-tiny takes its place.";
  assert_eq!(message, want);
  assert_eq!(fake.log().settings.model, None, "the first one left");
  assert_eq!(h.app().settings.model, None);
  assert!(h.rect(Id::of((USE, "whisper-tiny"))).is_none(), "in use");
  assert!(h.rect(Id::of((DOWNLOAD, "parakeet-v3"))).is_some(), "downloadable again");
  assert!(fake.log().checks > checks, "the session looks again");

  let message = delete(&mut h, "whisper-tiny");
  assert!(message.ends_with("It is the only model: dictation stops until you download another."));
  tab(&mut h, Page::General);
  let caption = h.text(row(MODEL).key("caption"));
  assert_eq!(caption, Some("Dictation needs a speech model: download one to start."));
}

/// The chosen model's file deleted behind the window's back: the next frame sees it, looks at the
/// models again (and asks the session to), and its row turns into the model not on disk, in its
/// place, as high, with Download; General says what transcribes instead. Downloaded again, it is
/// in use again.
#[test]
fn a_model_gone_behind_the_windows_back_shows_not_on_disk() {
  let home = Home::new("window-model-gone", &["parakeet-v3", "whisper-tiny"]);
  let fake = Fake::default();
  let settings = Settings { model: Some("parakeet-v3".into()), ..Settings::default() };
  let mut h = harness(&fake, settings, &home);
  tab(&mut h, Page::Models);
  let row_id = Id::of((page::ROW, "parakeet-v3"));
  let height = h.rect(row_id).unwrap().h;
  let checks = fake.log().checks;

  std::fs::remove_file(home.hub.find("parakeet-v3").unwrap()).unwrap();
  h.move_to(10.0, 300.0);
  h.frame();
  h.frame();
  assert!(fake.log().checks > checks, "the session is asked to look");
  assert_eq!(h.rect(row_id).map(|r| r.h), Some(height), "in its place, as high");
  let caption = h.text(row_id.key("caption")).unwrap().to_owned();
  assert!(caption.starts_with("Not on disk · "), "{caption}");
  assert!(h.rect(Id::of((DOWNLOAD, "parakeet-v3"))).is_some());
  assert!(h.rect(Id::of((USE, "whisper-tiny"))).is_none(), "it transcribes instead");

  tab(&mut h, Page::General);
  let instead = "parakeet-tdt-0.6b-v3 is not on disk: whisper-tiny transcribes instead.";
  assert_eq!(h.text(row(MODEL).key("caption")), Some(instead));
  let (items, at) = h.app().model_choices();
  assert_eq!(items[at], "parakeet-tdt-0.6b-v3 (not on disk)");

  // Downloaded again: in use again, on the next look.
  crate::testing::download(&home.hub, "parakeet-v3");
  tab(&mut h, Page::Models);
  assert!(h.rect(Id::of((page::DELETE, "parakeet-v3"))).is_some());
  assert!(h.rect(Id::of((USE, "parakeet-v3"))).is_none(), "in use");
  assert!(h.rect(Id::of((USE, "whisper-tiny"))).is_some());
}

/// Every row of the Models list has one height in every state: in use, downloaded, deleting,
/// available, downloading (its bar in place of the caption), preparing, failed (a long error cut
/// to one line), and the chosen model gone from the disk. A plate's first row has no hairline
/// above it: a point less.
#[test]
fn model_rows_keep_one_height_in_every_state() {
  let home = Home::new("window-row-heights", &["parakeet-v3", "parakeet-v2", "whisper-turbo"]);
  let fake = Fake::default();
  let running = Download::Running { done: 1, total: 2, rate: Some(3.2e6) };
  let error = "connection reset by peer while reading the response body from huggingface.co, \
    after three retries";
  let mut log = fake.log();
  log.states.insert("whisper-small", running);
  log.states.insert("whisper-base", Download::Preparing);
  log.states.insert("gigaam-v3-ctc", Download::Failed(error.into()));
  drop(log);
  let in_use = Settings { model: Some("parakeet-v2".into()), ..Settings::default() };
  let mut h = harness(&fake, in_use, &home);
  tab(&mut h, Page::Models);
  h.app_mut().models.deleting.push("whisper-turbo");
  h.rt.waker().wake();
  h.frame();
  let row = |h: &Harness<Prefs>, name: &str| {
    h.rect(Id::of((page::ROW, name))).unwrap_or_else(|| panic!("{name}"))
  };
  let idle = row(&h, "moonshine-tiny").h;
  assert_eq!(row(&h, "parakeet-v3").h, idle - 1.0, "the first, downloaded");
  let states = [
    ("parakeet-v2", "in use"),
    ("whisper-turbo", "deleting"),
    ("whisper-small", "downloading"),
    ("whisper-base", "preparing"),
    ("gigaam-v3-ctc", "failed"),
  ];
  for (name, state) in states {
    assert_eq!(row(&h, name).h, idle, "{name}, {state}");
  }

  // The bar where the caption was, how far it is beside it.
  let caption = |h: &Harness<Prefs>, name: &str| {
    h.text(Id::of((page::ROW, name)).key("caption")).map(str::to_owned)
  };
  assert_eq!(caption(&h, "whisper-small"), None, "no caption while downloading");
  assert!(caption(&h, "gigaam-v3-ctc").unwrap().starts_with("Could not download: connection"));
  assert!(h.rect(Id::of((CANCEL, "whisper-small"))).is_some());

  // The chosen model gone from the disk: in its place among the downloaded, as high.
  let gone = Settings { model: Some("whisper-tiny".into()), ..Settings::default() };
  h.app_mut().update(&gone, &status(Phase::Idle), 1);
  h.rt.waker().wake();
  h.frame();
  h.frame();
  assert_eq!(row(&h, "whisper-tiny").h, idle, "not on disk");
  assert!(caption(&h, "whisper-tiny").unwrap().starts_with("Not on disk · "));
}

/// A control keeps its width whatever its row says: General's Model pop-up with and without the
/// warning under it, Models' Download with and without a failure.
#[test]
fn controls_keep_their_width_whatever_the_row_says() {
  let home = Home::new("window-widths", &TWO);
  let fake = Fake::default();
  let settings = Settings { model: Some(TWO[0].into()), ..Settings::default() };
  let mut h = harness(&fake, settings.clone(), &home);
  let model = h.rect(Id::of(MODEL)).unwrap();
  let mut st = status(Phase::Idle);
  let error = "the GPU is out of memory while loading the encoder's weights".into();
  st.no_model = Some(NoModel::LoadFailed { model: "parakeet-tdt-0.6b-v3".into(), error });
  h.app_mut().update(&settings, &st, 1);
  h.rt.waker().wake();
  h.frame();
  h.frame();
  let caption = h.text(row(MODEL).key("caption")).unwrap_or_default().to_owned();
  assert!(caption.starts_with("parakeet-tdt-0.6b-v3 could not be loaded"), "{caption}");
  let after = h.rect(Id::of(MODEL)).unwrap();
  assert_eq!((after.w, after.x + after.w), (model.w, model.x + model.w), "same width and edge");

  tab(&mut h, Page::Models);
  let download = h.rect(Id::of((DOWNLOAD, "whisper-tiny"))).unwrap();
  fake.log().states.insert("whisper-tiny", Download::Failed("timed out".repeat(20)));
  h.rt.waker().wake();
  h.frame();
  let retry = h.rect(Id::of((DOWNLOAD, "whisper-tiny"))).unwrap();
  assert_eq!(retry.x + retry.w, download.x + download.w, "the right edge stays");
  let info = |h: &Harness<Prefs>| h.rect(Id::of((INFO, "whisper-tiny"))).unwrap();
  assert_eq!(info(&h).x, h.rect(Id::of((INFO, "whisper-base"))).unwrap().x);
}

#[test]
fn the_history_selects_copies_and_deletes() {
  let home = Home::new("window-history", &TWO);
  let ids = dictations(home.path());
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  tab(&mut h, Page::History);
  for id in &ids {
    assert!(h.rect(Id::of((COPY, *id))).is_some(), "entry {id}");
  }

  // Copy, to the window's clipboard (the harness's own, not the system's).
  let newest = *ids.last().unwrap();
  click(&mut h, Id::of((COPY, newest)));
  let copied = h.rt.clipboard().read();
  assert_eq!(
    copied.as_deref(),
    Some("The build is green again, the flaky test was a race in the pool.")
  );
  assert_eq!(h.app().history.selected, None, "the copy didn't select the row");

  // A click on a row's text selects it, tinted, at the same height; again unselects it.
  let row_id = Id::of((ROW, ids[0]));
  let before = h.rect(row_id).unwrap();
  let tint = |h: &mut Harness<Prefs>| h.render().pixel(before.x + 4.0, before.y + 4.0);
  h.leave();
  let plain = tint(&mut h);
  let text = h.rect(Id::of((TEXT, ids[0]))).expect("the text's cover");
  h.click(text.x + 40.0, center(text).y);
  h.frame();
  h.frame();
  assert_eq!(h.app().history.selected, Some(ids[0]));
  assert_eq!(h.rect(row_id), Some(before), "not opened");
  h.leave();
  assert_ne!(tint(&mut h), plain, "tinted");
  h.click(before.x + before.w - 2.0, center(before).y);
  h.frame();
  h.frame();
  assert_eq!(h.app().history.selected, None);

  // Resting on the text shows the whole of it.
  let tip = Id::of((TEXT, ids[0])).key("tip");
  h.move_to(text.x + 40.0, center(text).y);
  h.wait(Duration::from_millis(1500));
  assert!(h.rect(tip).is_some(), "the text's tooltip");

  // Delete asks in the dialog with the start of the text; Escape keeps it, Delete deletes it.
  click(&mut h, Id::of((DELETE, ids[0])));
  let ask = h.app().ask.clone().expect("the dialog");
  let want = "“Let's move the design review to Thursday afternoon.” is removed from the history.";
  assert_eq!(ask.question(), ("Delete this dictation?".into(), want.into()));
  h.key(NamedKey::Escape, Modifiers::empty());
  h.frame();
  assert!(h.app().ask.is_none() && fake.log().deleted.is_empty());
  click(&mut h, Id::of((DELETE, ids[0])));
  h.wait(Duration::from_millis(200));
  click(&mut h, Id::of(CONFIRM).key("action"));
  assert_eq!(fake.log().deleted, [ids[0]]);
  assert!(h.rect(Id::of((COPY, ids[0]))).is_none(), "gone from the list");

  // The search finds by text, in any case.
  click(&mut h, Id::of(SEARCH));
  h.type_text("INVOICE");
  h.frame();
  h.frame();
  assert!(h.rect(Id::of((COPY, ids[1]))).is_some());
  assert!(h.rect(Id::of((COPY, newest))).is_none());
}

/// Re-transcribe asks the session for an entry with its recording, and waits while it runs or
/// while no model can. An entry without text is listed, with nothing to copy; one transcribed
/// again offers Undo.
#[test]
fn a_dictation_is_transcribed_again() {
  let home = Home::new("window-retranscribe", &TWO);
  let ids = history_of(home.path(), &[("Hello there.", 1, 1.5), ("", 2, 1.0)], true);
  let (id, untranscribed) = (ids[0], ids[1]);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  tab(&mut h, Page::History);
  assert_eq!(hover(&mut h, Id::of((COPY, untranscribed))), (Cursor::Arrow, 0), "nothing to copy");
  assert!(h.rect(Id::of((UNDO, id))).is_none(), "nothing to undo yet");
  click(&mut h, Id::of((RETRANSCRIBE, untranscribed)));
  click(&mut h, Id::of((RETRANSCRIBE, id)));
  assert_eq!(fake.log().retranscribed, [untranscribed, id]);

  // Its new text came: Undo puts back the one before.
  let history = History::open(home.path()).unwrap();
  history.db.update_text(id, "Hello, there.", "whisper-tiny-Q8_0", None).unwrap();
  let settings = h.app().settings.clone();
  h.app_mut().update(&settings, &status(Phase::Idle), 2);
  h.rt.waker().wake();
  h.frame();
  click(&mut h, Id::of((UNDO, id)));
  assert_eq!(fake.log().undone, [id]);

  let mut st = status(Phase::Idle);
  st.retranscribing = vec![id];
  h.app_mut().update(&settings, &st, 3);
  h.rt.waker().wake();
  h.frame();
  assert_eq!(hover(&mut h, Id::of((RETRANSCRIBE, id))), (Cursor::Arrow, 0));
  assert_eq!(hover(&mut h, Id::of((DELETE, id))), (Cursor::Arrow, 0), "nor deleted meanwhile");

  // A delete the session refused: the entry is back, and why is said.
  h.app_mut().update(&settings, &status(Phase::Idle), 4);
  h.app_mut().delete_entry(id);
  h.app_mut().entry_not_deleted("entry 1 is being transcribed");
  h.rt.waker().wake();
  h.frame();
  assert!(h.rect(Id::of((COPY, id))).is_some(), "back");
  assert_eq!(
    h.app().history.note.as_deref(),
    Some("Couldn't delete: entry 1 is being transcribed")
  );
}

/// The first launch: General shows what is missing, each with Allow, and with no model a button to
/// the Models page in place of the model pop-up. A permission answered gets Open Settings.
#[test]
fn general_guides_the_first_launch() {
  let home = Home::new("window-first-run", &[]);
  let fake = Fake::default();
  fake.log().access = Some(Access::default());
  let mut h = harness(&fake, Settings::default(), &home);
  assert!(h.rect(Id::of(MODEL)).is_none(), "no pop-up without a model");
  let caption = h.text(row(MODEL).key("caption"));
  assert_eq!(caption, Some("Dictation needs a speech model: download one to start."));

  // The buttons' labels by their widths: Allow is the shorter.
  let widths = |h: &Harness<Prefs>| {
    [OPEN_ACCESSIBILITY, OPEN_MICROPHONE].map(|b| h.rect(Id::of(b)).unwrap().w)
  };
  let allow = widths(&h);
  click(&mut h, Id::of(OPEN_ACCESSIBILITY));
  click(&mut h, Id::of(OPEN_MICROPHONE));
  assert_eq!(fake.log().allowed, [Pane::Accessibility, Pane::Microphone]);

  // Answered, back in the window: a denied microphone is not asked again, only System Settings
  // changes it.
  let given = Access { accessibility: true, microphone: Grant::Denied, ..Access::default() };
  fake.log().access = Some(given);
  h.app_mut().access_changed();
  h.frame();
  let open = widths(&h);
  assert!(open[0] > allow[0] && open[1] > allow[1], "Open Settings: {allow:?} {open:?}");

  click(&mut h, Id::of(DOWNLOAD_MODEL));
  assert_eq!(h.app().page, Page::Models);
}

/// Every page in both looks, as people see them: two models downloaded and one downloading, a
/// history of today's and yesterday's dictations with one selected; written to `WAVO_SHOTS` when
/// it is set. General and Models also with an English-only model in use and with the chosen model
/// gone; Models with its delete asked.
#[test]
fn every_page_renders_in_both_looks() {
  let home =
    Home::new("window-shots", &["parakeet-v3", "parakeet-v2", "whisper-turbo", "gigaam-v3"]);
  let ids = many_dictations(home.path());
  let dir = std::env::var_os("WAVO_SHOTS").map(PathBuf::from);
  let shot = |h: &mut Harness<Prefs>, name: &str| {
    h.leave();
    h.wait(Duration::from_millis(400));
    if let Some(dir) = &dir {
      std::fs::create_dir_all(dir).unwrap();
      h.shot(&dir.join(format!("{name}.png"))).unwrap();
    }
  };
  for (look, appearance) in [("light", Appearance::LIGHT), ("dark", Appearance::DARK)] {
    for (page, _, name) in PAGES {
      let fake = Fake::default();
      fake.log().recordings_size = Some(184_300_000);
      fake.log().handy = Some(handy_summary(&home));
      let running = Download::Running { done: 114_000_000, total: 272_000_000, rate: Some(3.2e6) };
      fake.log().states.insert("gigaam-v3-e2e-ctc", running);
      let mut h = harness(&fake, Settings::default(), &home);
      h.event(Event::Appearance(appearance));
      tab(&mut h, page);
      if page == Page::History {
        click(&mut h, Id::of((TEXT, ids[7])));
      }

      h.leave();
      h.wait(Duration::from_millis(400));
      // The window's ground under the toolbar's edge: dark in the dark look.
      let ground = h.render().pixel(4.0, 100.0);
      assert_eq!(ground[0] < 100, appearance.dark, "{name} {look}: {ground:?}");
      shot(&mut h, &format!("{}-{look}", name.to_lowercase()));
      match page {
        Page::General => {
          let empty = Home::new("window-shots-first-run", &[]);
          let first = Fake::default();
          first.log().access = Some(Access::default());
          let mut first_run = harness(&first, Settings::default(), &empty);
          first_run.event(Event::Appearance(appearance));
          shot(&mut first_run, &format!("general-first-run-{look}"));

          click(&mut h, Id::of(MODEL));
          click(&mut h, Id::of(MODEL).key(("item", 1)));
          assert!(h.app().model_note().is_some(), "{look}: the note");
          shot(&mut h, &format!("general-english-only-{look}"));
          tab(&mut h, Page::Models);
          shot(&mut h, &format!("models-english-only-{look}"));

          let gone = Settings { model: Some("whisper-tiny".into()), ..Settings::default() };
          let mut h = harness(&fake, gone, &home);
          h.event(Event::Appearance(appearance));
          for (page, name) in [(Page::General, "general"), (Page::Models, "models")] {
            tab(&mut h, page);
            assert!(h.app().model_problem().is_some(), "{look}: {name}");
            shot(&mut h, &format!("{name}-model-gone-{look}"));
          }
        }
        Page::Models => {
          click(&mut h, Id::of((page::DELETE, "whisper-turbo")));
          assert!(h.rect(Id::of(CONFIRM).key("card")).is_some(), "{look}: the dialog");
          shot(&mut h, &format!("models-delete-{look}"));
        }
        Page::Advanced => {
          click(&mut h, Id::of(data::IMPORT));
          let tell = fake.log().import.take().expect("the import asked for");
          tell(data::Import::Copying(420, 1_180));
          h.rt.waker().wake();
          shot(&mut h, &format!("advanced-importing-{look}"));
        }
        _ => {}
      }
    }
  }
}

/// Handy's history as a shot shows it: a year and more of dictations.
fn handy_summary(home: &Home) -> Summary {
  let (first, last) = (Some(now_ms() - 400 * 86_400_000), Some(now_ms() - 86_400_000));
  let dir = home.path().join("handy");
  Summary { dir, entries: 1_204, recordings: 1_180, bytes: 2_100_000_000, first, last }
}

/// Advanced's Data section: Import from Handy only when Handy's history is here, saying what it
/// would bring, its copies' progress in place of that while it runs, then how it went; Export
/// writes an archive into Downloads and says so.
#[test]
fn handys_history_is_imported_and_the_history_exported() {
  let home = Home::new("window-data", &TWO);
  let fake = Fake::default();
  let mut h = harness(&fake, Settings::default(), &home);
  tab(&mut h, Page::Advanced);
  assert!(h.rect(Id::of(data::IMPORT)).is_none(), "no Handy here");

  fake.log().handy = Some(Summary { first: None, last: None, ..handy_summary(&home) });
  tab(&mut h, Page::General);
  h.app_mut().refresh();
  tab(&mut h, Page::Advanced);
  let caption = |h: &Harness<Prefs>, id| h.text(row(id).key("caption")).map(str::to_owned);
  let found = caption(&h, data::IMPORT).expect("Handy found");
  assert_eq!(found, "1,204 dictations, 1,180 recordings · 2.1 GB");
  let at = |h: &Harness<Prefs>| h.rect(Id::of(data::IMPORT)).unwrap();
  let before = at(&h);
  click(&mut h, Id::of(data::IMPORT));
  let tell = fake.log().import.take().expect("the import asked for");
  tell(data::Import::Copying(120, 900));
  h.rt.waker().wake();
  h.frame();
  assert_eq!(at(&h), before, "the button stays in place");
  assert_eq!(caption(&h, data::IMPORT), None, "the progress in its place");
  assert_eq!(hover(&mut h, Id::of(data::IMPORT)), (Cursor::Arrow, 0), "disabled while it runs");
  tell(data::Import::Done(Ok(Report { entries: 1_204, added: 1_200, copied: 1_180, failed: 0 })));
  h.rt.waker().wake();
  h.frame();
  h.frame();
  let done = "Imported 1,200 dictations (4 were here already), 1,180 recordings.";
  assert_eq!(caption(&h, data::IMPORT).as_deref(), Some(done));
  assert_eq!(at(&h), before, "the result line in place of the caption");

  let export = caption(&h, data::EXPORT).unwrap_or_default();
  assert!(export.contains("wavo-app import <file>"), "{export}");
  click(&mut h, Id::of(data::EXPORT));
  let file = fake.log().exports.pop().expect("exported");
  assert!(file.parent().is_some_and(|d| d.ends_with("Downloads")), "{file:?}");
  let name = file.file_name().unwrap().to_string_lossy().into_owned();
  assert!(name.starts_with("wavo-export-") && name.ends_with(".tar"), "{name}");
  let exported = caption(&h, data::EXPORT).unwrap_or_default();
  assert!(exported.ends_with(" · 3 dictations · 1 MB"), "{exported}");
}
