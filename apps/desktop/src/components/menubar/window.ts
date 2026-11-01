// What the widget's buttons do to windows. Inside Tauri: the `open_main_window` and
// `hide_menu_bar` commands (the tray owns the windows). In a plain browser (mock bridge,
// screenshots, E2E): "Open scull" goes to the main app's page on that route, and hiding does
// nothing (there is no tray).
import type { InjectionKey } from 'vue';
import type { ScullBridge } from '../../bridge';
import { createTauriWindowCommands } from '../../bridge/tauri';
import { MAIN_ROUTES, type MainRoute } from './model';

export interface WidgetWindow {
  readonly kind: 'tauri' | 'browser';
  openMain(route: MainRoute): Promise<void>;
  hide(): Promise<void>;
}

export function browserWidgetWindow(loc: { assign(url: string): void } = globalThis.location): WidgetWindow {
  return {
    kind: 'browser',
    async openMain(route) {
      loc.assign(`./index.html#${route}`);
    },
    async hide() {},
  };
}

export function tauriWidgetWindow(cmds = createTauriWindowCommands()): WidgetWindow {
  return {
    kind: 'tauri',
    openMain: (route) => cmds.openMain(route),
    hide: () => cmds.hideWidget(),
  };
}

export function widgetWindowFor(bridge: ScullBridge): WidgetWindow {
  return bridge.kind === 'tauri' ? tauriWidgetWindow() : browserWidgetWindow();
}

/** Lets tests (and the entry point) provide the window actions. */
export const WIDGET_WINDOW_KEY: InjectionKey<WidgetWindow> = Symbol('scull-widget-window');

/** Whether `route` is one the widget may open in the main window. */
export function isMainRoute(route: unknown): route is MainRoute {
  return typeof route === 'string' && (MAIN_ROUTES as readonly string[]).includes(route);
}
