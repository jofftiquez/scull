import { flushPromises, mount } from '@vue/test-utils';
import { defineComponent, h, ref } from 'vue';
import { describe, expect, it, vi } from 'vitest';
import { createMemoryHistory, createRouter } from 'vue-router';
import ScreenLayout from './ScreenLayout.vue';
import ActionFooter from './ActionFooter.vue';
import ActionConfirm from './ActionConfirm.vue';
import AppHeader from './AppHeader.vue';
import { core } from '../../state/core';
import { BRIDGE_KEY } from '../../bridge';
import { createMockBridge } from '../../bridge/mock';

describe('dashboard action placement', () => {
  it('keeps page and detail actions outside scroll regions without losing their handlers', async () => {
    const count = ref(0);
    const shown = ref(true);
    const Content = defineComponent({ setup: () => () => h('div', [
      h('button', { 'data-testid': 'hide', onClick: () => { shown.value = false; } }, 'Hide action'),
      shown.value ? h(ActionFooter, {}, { default: () => h('button', { 'data-testid': 'page-action', onClick: () => { count.value++; } }, `Apply ${count.value}`) }) : null,
    ]) });
    const router = createRouter({ history: createMemoryHistory(), routes: [{ path: '/', component: Content }] });
    await router.push('/');
    const w = mount(ScreenLayout, { global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge() } }, slots: {
      default: Content,
      aside: () => h(ActionFooter, { panel: 'aside' }, { default: () => h('button', { 'data-testid': 'detail-action' }, 'Inspect') }),
    } });
    await flushPromises();
    expect(w.get('[data-testid="main-scroll"]').find('[data-testid="page-action"]').exists()).toBe(false);
    expect(w.get('[data-testid="aside-scroll"]').find('[data-testid="detail-action"]').exists()).toBe(false);
    await w.get('[data-testid="main-footer"] [data-testid="page-action"]').trigger('click');
    expect(count.value).toBe(1);
    const footer = w.get('[data-testid="main-footer"]');
    expect(footer.text()).toContain('Inspect');
    expect(footer.element.parentElement).toBe(w.element);
    expect(footer.element.previousElementSibling?.classList.contains('screen-panels')).toBe(true);
    expect(w.find('[data-testid="aside-footer"]').exists()).toBe(false);
    await w.get('[data-testid="hide"]').trigger('click');
    expect(w.get('[data-testid="main-footer"]').text()).toBe('Inspect');
    await w.setProps({ detailOpen: false });
    expect(w.get('[data-testid="main-footer"]').text()).toBe('');
    w.unmount();
  });

  it('renders actions inline when used outside the dashboard shell', () => {
    const w = mount(ActionFooter, { slots: { default: '<button>Apply</button>' } });
    expect(w.get('button').text()).toBe('Apply');
    w.unmount();
  });
});


describe('shell feedback', () => {
  it('announces loading in the content region while keeping the toolbar visible', async () => {
    const router = createRouter({ history: createMemoryHistory(), routes: [{ path: '/', component: { template: '<div />' } }] });
    await router.push('/');
    const w = mount(ScreenLayout, { props: { loading: true, loadingLabel: 'Loading projects…' }, global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge() } }, slots: { default: '<p>No projects found</p>' } });
    expect(w.get('[data-testid="main-scroll"]').attributes('aria-busy')).toBe('true');
    expect(w.get('[data-testid="loading-state"]').text()).toBe('Loading projects…');
    expect(w.get('[data-testid="loading-state"]').attributes('role')).toBe('status');
    expect(w.get('[data-testid="header"]').isVisible()).toBe(true);
    expect(w.text()).not.toContain('No projects found');
    await w.setProps({ loading: false });
    expect(w.get('[data-testid="main-scroll"]').attributes('aria-busy')).toBe('false');
    expect(w.find('[data-testid="loading-state"]').exists()).toBe(false);
    expect(w.text()).toContain('No projects found');
    w.unmount();
  });

  it('keeps scan state visible and disables repeated requests', async () => {
    const router = createRouter({ history: createMemoryHistory(), routes: [{ path: '/', component: { template: '<div />' } }] });
    await router.push('/');
    const w = mount(AppHeader, { props: { busy: false }, global: { plugins: [router], provide: { [BRIDGE_KEY as symbol]: createMockBridge() } } });
    const scan = w.get('[data-testid="rescan"]');
    expect(scan.attributes('aria-busy')).toBe('false');
    expect(scan.classes()).not.toContain('is-scanning');
    await w.setProps({ busy: true });
    expect(scan.attributes('aria-busy')).toBe('true');
    expect(scan.attributes('disabled')).toBeDefined();
    expect(scan.classes()).toContain('is-scanning');
    expect(scan.text()).toBe('Scanning…');
    await w.setProps({ busy: false });
    core.scanning = true;
    await flushPromises();
    expect(scan.classes()).toContain('is-scanning');
    expect(w.text()).toContain('Cancel scan');
    core.scanning = false;
    w.unmount();
  });

  it('opens a native confirmation safely and requires the irreversible count', async () => {
    const showModal = vi.fn();
    const close = vi.fn();
    Object.defineProperties(HTMLDialogElement.prototype, {
      showModal: { configurable: true, value: showModal },
      close: { configurable: true, value: close },
    });
    const w = mount(ActionConfirm, { props: { title: 'Remove item?', label: 'Remove', count: 2 } });
    expect(showModal).toHaveBeenCalledOnce();
    const confirm = w.get('[data-testid="confirm-action"]');
    expect(confirm.attributes('disabled')).toBeDefined();
    await w.get('input').setValue('1');
    expect(confirm.attributes('disabled')).toBeDefined();
    await w.get('input').setValue('2');
    await confirm.trigger('click');
    expect(w.emitted('confirm')).toHaveLength(1);
    await w.get('dialog').trigger('cancel');
    expect(w.emitted('cancel')).toHaveLength(1);
    w.unmount();
    expect(close).toHaveBeenCalledOnce();
    Reflect.deleteProperty(HTMLDialogElement.prototype, 'showModal');
    Reflect.deleteProperty(HTMLDialogElement.prototype, 'close');
  });
});
