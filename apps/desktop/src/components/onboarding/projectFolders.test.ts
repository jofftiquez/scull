import { flushPromises, mount } from '@vue/test-utils';
import { describe, expect, it } from 'vitest';
import ProjectFolders from './ProjectFolders.vue';

function folders(pickFolders: () => Promise<string[] | null>) {
  const w = mount(ProjectFolders, { props: {
    home: '/Users/ada', folders: [], roots: [], confirmed: [], pickFolders,
    'onUpdate:roots': (roots: string[]) => { void w.setProps({ roots }); },
    'onUpdate:confirmed': (confirmed: string[]) => { void w.setProps({ confirmed }); },
  } });
  return w;
}

describe('native folder selection', () => {
  it('adds chosen folders once and cancellation leaves them unchanged', async () => {
    let result: string[] | null = ['/Users/ada/My Projects', '/Users/ada/My Projects'];
    const w = folders(async () => result);
    await w.get('[data-testid="choose-folders"]').trigger('click');
    await flushPromises();
    expect(w.props('roots')).toEqual(['/Users/ada/My Projects']);
    expect(w.findAll('[data-testid="folder"]')).toHaveLength(1);
    result = null;
    await w.get('[data-testid="choose-folders"]').trigger('click');
    await flushPromises();
    expect(w.props('roots')).toEqual(['/Users/ada/My Projects']);
    expect(w.find('input[type="text"]').exists()).toBe(false);
    w.unmount();
  });

  it('removes a folder from both the selection and displayed list', async () => {
    const w = folders(async () => ['/Users/ada/code']);
    await w.get('[data-testid="choose-folders"]').trigger('click');
    await flushPromises();
    await w.get('button[aria-label="Remove ~/code"]').trigger('click');
    expect(w.props('roots')).toEqual([]);
    expect(w.findAll('[data-testid="folder"]')).toHaveLength(0);
    w.unmount();
  });

  it('confirms multiple broad folders individually without losing safe folders', async () => {
    const w = folders(async () => ['/Users/ada', '/Users', '/Users/ada/code']);
    await w.get('[data-testid="choose-folders"]').trigger('click');
    await flushPromises();
    expect(w.props('roots')).toEqual(['/Users/ada/code']);
    expect(w.get('[data-testid="broad-confirm"]').text()).toContain('is your home folder');
    await w.get('[data-testid="confirm-broad"]').trigger('click');
    expect(w.get('[data-testid="broad-confirm"]').text()).toContain('contains your home folder');
    await w.get('[data-testid="decline-broad"]').trigger('click');
    expect(w.props('confirmed')).toEqual(['/Users/ada']);
    expect(w.props('roots')).toEqual(['/Users/ada', '/Users/ada/code']);
    w.unmount();
  });

  it('prevents duplicate dialogs and ignores a picker that finishes after unmount', async () => {
    let resolve!: (paths: string[]) => void;
    const w = folders(() => new Promise((r) => { resolve = r; }));
    await w.get('[data-testid="choose-folders"]').trigger('click');
    expect(w.get('[data-testid="choose-folders"]').attributes('disabled')).toBeDefined();
    w.unmount();
    resolve(['/Users/ada/code']);
    await flushPromises();
    expect(w.emitted('update:roots')).toBeUndefined();
  });

  it('shows picker failures with a way to try again', async () => {
    const w = folders(async () => { throw new Error('dialog unavailable'); });
    await w.get('[data-testid="choose-folders"]').trigger('click');
    await flushPromises();
    expect(w.get('[role="alert"]').text()).toContain('dialog unavailable');
    expect(w.get('[data-testid="choose-folders"]').attributes('disabled')).toBeUndefined();
    w.unmount();
  });
});
