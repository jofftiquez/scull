import { createApp } from 'vue';
import { createRouter, createWebHashHistory } from 'vue-router';
// Fonts are bundled (no network at runtime): DM Sans for text, DM Mono for paths and figures.
import '@fontsource/dm-sans/400.css';
import '@fontsource/dm-sans/500.css';
import '@fontsource/dm-sans/600.css';
import '@fontsource/dm-sans/700.css';
import '@fontsource/dm-mono/400.css';
import '@fontsource/dm-mono/500.css';
import './styles/main.css';
import App from './App.vue';
import { BRIDGE_KEY, selectBridge } from './bridge';
import { routes } from './navigation';

const router = createRouter({ history: createWebHashHistory(), routes });

createApp(App).provide(BRIDGE_KEY, selectBridge()).use(router).mount('#app');
