import js from '@eslint/js';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  { ignores: ['dist/**', 'e2e/.dist/**', 'e2e/results/**', 'e2e/report/**', 'node_modules/**', 'src-tauri/**', 'bridge/**', 'docker/**'] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    // TypeScript checks undefined names (tsc); eslint's no-undef doesn't know DOM types.
    files: ['**/*.ts', '**/*.tsx'],
    rules: { 'no-undef': 'off' },
  },
  {
    files: ['scripts/**/*.mjs'],
    languageOptions: { globals: { process: 'readonly', console: 'readonly' } },
  },
  {
    rules: {
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
