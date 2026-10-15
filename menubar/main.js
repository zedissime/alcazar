const { app, Tray, Menu, BrowserWindow, screen, globalShortcut } = require('electron');
const path = require('path');
const fs = require('fs');

const DASHBOARD_URL = process.env.ALCAZAR_DASHBOARD_URL || 'http://127.0.0.1:7777';
const STATE_FILE = path.join(app.getPath('userData'), 'pinned-windows.json');

let tray = null;
let dashboardWin = null;
const pinnedWindows = new Map(); // name -> BrowserWindow

function loadPinnedState() {
  try {
    return JSON.parse(fs.readFileSync(STATE_FILE, 'utf8'));
  } catch {
    return {};
  }
}

function savePinnedState() {
  const state = {};
  for (const [name, win] of pinnedWindows) {
    state[name] = win.getBounds();
  }
  fs.writeFileSync(STATE_FILE, JSON.stringify(state, null, 2));
}

async function fetchProcesses() {
  try {
    const res = await fetch(`${DASHBOARD_URL}/api/processes`);
    return await res.json();
  } catch (err) {
    console.error('failed to reach alcazar dashboard:', err.message);
    return [];
  }
}

function openDashboard() {
  if (dashboardWin && !dashboardWin.isDestroyed()) {
    dashboardWin.isVisible() ? dashboardWin.hide() : dashboardWin.show();
    return;
  }
  dashboardWin = new BrowserWindow({
    width: 1100,
    height: 750,
    title: 'alcazar',
    webPreferences: { contextIsolation: true },
  });
  dashboardWin.loadURL(DASHBOARD_URL);
  dashboardWin.on('closed', () => { dashboardWin = null; });
}

function currentDisplayBounds() {
  // Whichever screen the mouse is on, not always the primary one -- so
  // pinning from a menu bar on a secondary display doesn't land the window
  // on the wrong screen.
  const point = screen.getCursorScreenPoint();
  return screen.getDisplayNearestPoint(point).workArea;
}

function pinLogWindow(name, bounds) {
  if (pinnedWindows.has(name)) return pinnedWindows.get(name);

  const win = new BrowserWindow({
    ...(bounds || currentDisplayBounds()),
    title: `alcazar: ${name}`,
    frame: true,
    fullscreenable: false,
    webPreferences: { contextIsolation: true },
  });

  // Keep the window above everything, including apps running in a macOS
  // full-screen space -- without `visibleOnFullScreen: true` the window
  // simply disappears the moment another app goes full screen.
  win.setAlwaysOnTop(true, 'screen-saver');
  win.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });

  win.loadURL(`${DASHBOARD_URL}/?pin=${encodeURIComponent(name)}`);
  win.on('closed', () => {
    pinnedWindows.delete(name);
    savePinnedState();
    buildMenu();
  });

  pinnedWindows.set(name, win);
  savePinnedState();
  return win;
}

function unpinLogWindow(name) {
  const win = pinnedWindows.get(name);
  if (win) win.close(); // triggers the 'closed' handler above
}

function togglePin(name) {
  if (pinnedWindows.has(name)) {
    unpinLogWindow(name);
  } else {
    pinLogWindow(name);
  }
  buildMenu();
}

async function buildMenu() {
  const processes = await fetchProcesses();
  const pinItems = processes.length
    ? processes.map((p) => ({
        label: p.name,
        type: 'checkbox',
        checked: pinnedWindows.has(p.name),
        click: () => togglePin(p.name),
      }))
    : [{ label: '(cannot reach alcazar daemon)', enabled: false }];

  tray.setContextMenu(Menu.buildFromTemplate([
    { label: 'Open Dashboard', click: openDashboard },
    { type: 'separator' },
    { label: 'Pin log window', submenu: pinItems },
    { type: 'separator' },
    { label: 'Quit', role: 'quit' },
  ]));
}

// Only one tray icon should ever exist. If a second copy gets launched
// (e.g. `alcazar start --menubar` while the app is already running), just
// bring the existing dashboard forward and let the new copy exit instead
// of ending up with two tray icons fighting over the same pinned windows.
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => openDashboard());

  app.whenReady().then(async () => {
    app.dock?.hide(); // menu bar only, no Dock icon/app switcher entry

    tray = new Tray(path.join(__dirname, 'assets/trayIconTemplate.png'));
    tray.setToolTip('alcazar');
    tray.on('click', openDashboard);
    tray.on('right-click', buildMenu);
    await buildMenu();

    // System-wide, works even with no alcazar window focused (or none
    // open at all) -- that's the point of a menu bar app. Registered here
    // rather than as a BrowserWindow accelerator, which would only fire
    // while that specific window already had focus.
    if (!globalShortcut.register('CommandOrControl+Shift+I', openDashboard)) {
      console.error('alcazar-menubar: Cmd+Shift+I is already taken by another app, dashboard shortcut not registered');
    }

    // Restore windows that were pinned the last time this app quit.
    const saved = loadPinnedState();
    const processes = await fetchProcesses();
    const known = new Set(processes.map((p) => p.name));
    for (const [name, bounds] of Object.entries(saved)) {
      if (known.has(name)) pinLogWindow(name, bounds);
    }
    await buildMenu();
  });

  // macOS already keeps app.whenReady()'d apps alive with no windows open;
  // this is just an explicit no-op so it stays that way if that default
  // ever changes. Quitting only ever happens via the tray menu.
  app.on('window-all-closed', () => {});

  app.on('will-quit', () => globalShortcut.unregisterAll());
}
