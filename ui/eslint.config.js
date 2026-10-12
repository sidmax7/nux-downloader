import pluginVue from "eslint-plugin-vue";
import tseslint from "typescript-eslint";

export default [
  { ignores: ["dist/", "node_modules/", "src/bindings.ts"] },
  ...tseslint.configs.recommended,
  ...pluginVue.configs["flat/essential"],
  {
    files: ["**/*.vue"],
    languageOptions: { parserOptions: { parser: tseslint.parser } },
  },
];
