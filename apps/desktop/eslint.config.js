import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import vue from 'eslint-plugin-vue';
import vueParser from 'vue-eslint-parser';

export default tseslint.config(
  { ignores: ['dist/**', 'bench/dist/**', 'e2e/.dist/**', 'e2e/results/**', 'e2e/report/**', 'node_modules/**', 'src-tauri/**', 'bridge/**', 'docker/**'] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  ...vue.configs['flat/recommended'],
  {
    files: ['**/*.vue'],
    languageOptions: {
      parser: vueParser,
      parserOptions: { parser: tseslint.parser, extraFileExtensions: ['.vue'], sourceType: 'module' },
    },
  },
  {
    // TypeScript checks undefined names (vue-tsc); eslint's no-undef doesn't know DOM types.
    files: ['**/*.ts', '**/*.vue'],
    rules: { 'no-undef': 'off' },
  },
  {
    files: ['scripts/**/*.mjs'],
    languageOptions: { globals: { process: 'readonly', console: 'readonly' } },
  },
  {
    // Benchmark and audit drivers (node); page.evaluate callbacks run in the browser.
    files: ['bench/**/*.mjs', 'a11y/**/*.mjs'],
    languageOptions: {
      globals: {
        process: 'readonly',
        console: 'readonly',
        setTimeout: 'readonly',
        clearTimeout: 'readonly',
        URL: 'readonly',
        window: 'readonly',
        document: 'readonly',
        getComputedStyle: 'readonly',
        MutationObserver: 'readonly',
        SVGElement: 'readonly',
      },
    },
  },
  {
    rules: {
      // Formatting is not lint's job here; keep the rules about correctness.
      'vue/max-attributes-per-line': 'off',
      'vue/singleline-html-element-content-newline': 'off',
      'vue/html-self-closing': 'off',
      'vue/multi-word-component-names': 'off',
      '@typescript-eslint/consistent-type-imports': 'error',
      'no-restricted-imports': [
        'error',
        {
          paths: [
            {
              name: '@tauri-apps/api/core',
              message: 'Go through src/bridge (ScullBridge); only src/bridge/tauri.ts talks to Tauri.',
            },
            {
              name: '@tauri-apps/api/event',
              message: 'Go through src/bridge (ScullBridge); only src/bridge/tauri.ts talks to Tauri.',
            },
          ],
        },
      ],
    },
  },
  {
    files: ['src/bridge/tauri.ts'],
    rules: { 'no-restricted-imports': 'off' },
  },
);
