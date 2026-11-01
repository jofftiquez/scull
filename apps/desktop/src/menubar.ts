// Entry of the menu bar / tray widget (menubar.html; design 11, P4-UI-01). A page of its own,
// without the router and the shell, so the small window the tray opens loads only the widget.
import { createApp } from 'vue';
import '@fontsource/dm-sans/400.css';
import '@fontsource/dm-sans/500.css';
import '@fontsource/dm-sans/600.css';
import '@fontsource/dm-sans/700.css';
import '@fontsource/dm-mono/400.css';
import './styles/main.css';
import MenuBarWidget from './screens/MenuBar/MenuBarWidget.vue';
import { BRIDGE_KEY, selectBridge } from './bridge';

createApp(MenuBarWidget).provide(BRIDGE_KEY, selectBridge()).mount('#app');
