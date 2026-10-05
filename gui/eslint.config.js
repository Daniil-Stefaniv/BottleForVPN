import js from "@eslint/js";
import tseslint from "typescript-eslint";

// Линтер GUI (M4 Task 2): строгий recommended-набор для vanilla TS.
export default tseslint.config(
  { ignores: ["ui/dist", "node_modules"] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
);
