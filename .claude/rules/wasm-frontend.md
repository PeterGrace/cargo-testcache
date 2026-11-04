---
description: "Web-based front-end (WASM/dioxus) design and computation rules"
paths:
  - "**/*.html"
  - "**/*.css"
  - "**/*.scss"
  - "web/**"
  - "**/src/app.rs"
  - "**/src/components/**"
---

# WASM / Dioxus Front End

- If designing applications with a web-based front end interface, e.g. compiling to WASM or using `dioxus`:
  - All deep computation **MUST** occur within Rust processes (i.e. the WASM binary or the `dioxus` app Rust process). **NEVER** use JavaScript for deep computation.
  - The front-end **MUST** use Pico CSS and vanilla JavaScript. **NEVER** use jQuery or any component-based frameworks such as React.
  - The front-end should prioritize speed and common HID guidelines.
  - The app should use adaptive light/dark themes by default, with a toggle to switch the themes.
  - The typography/theming of the application **MUST** be modern and unique, similar to that of popular single-page web/mobile. **ALWAYS** add an appropriate font for headers and body text. You may reference fonts from Google Fonts.
  - **NEVER** use the Pico CSS defaults as-is: a separate CSS/SCSS file is encouraged. The design **MUST** logically complement the semantics of the application use case.
  - **ALWAYS** rebuild the WASM binary if any underlying Rust code that affects it is touched.
