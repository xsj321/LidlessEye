// TypeScript 6 turns on `noUncheckedSideEffectImports` by default, which rejects
// bare `import "./globals.css"` without a declaration. Next handles the CSS
// itself at build time; this only tells the checker the import is expected.
declare module "*.css";
