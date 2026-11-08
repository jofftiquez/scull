import type { InjectionKey, Ref } from 'vue';

export const FOOTER_TARGETS: InjectionKey<Ref<HTMLElement | null>> = Symbol('scull-footer-targets');
