# GPUI 0.2.2, patched

This is the crates.io release of GPUI 0.2.2 (Apache-2.0), minus its examples
and tests, with one change for artty:

- `src/platform/mac/metal_renderer.rs`, `build_pipeline_state`: the alpha
  channel blends with `OneMinusSourceAlpha` ("over") instead of `One`. With
  `One`, alphas added up, so on a translucent window everything drawn over
  the background (a hovered or selected sidebar row, the sidebar itself) came
  out more opaque than the window's opacity, and stacking made it worse.

To update GPUI, replace `src`, `resources`, `build.rs` and `Cargo.toml`
(dropping `[[example]]` and `[[test]]` tables) from the new release and
reapply the change above.
