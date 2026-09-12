# salmon-watch

Salmon Watch 2 reads the existing Salmon Watch configuration from
`$XDG_CONFIG_HOME/salmon-watch/salmon-watch.yml` (normally
`~/.config/salmon-watch/salmon-watch.yml`). A custom file can be selected with
`--config`.

The current implementation supports direct, unencrypted WebSocket connections:

```yaml
wsClient:
  servers:
    - id: local
      addr: localhost:41990
```

## Testing

Run the regular test suite with:

```sh
cargo test
```

The two native window-geometry tests are marked `#[ignore]`, so the regular
test suite compiles but does not run them. They require a real X11 graphical
session and window manager; a headless test environment cannot accurately test
window positioning, maximizing, hiding, and restoring.

Run each native test explicitly:

```sh
cargo test native_startup_restores_geometry_and_maximized_state -- --ignored
cargo test native_hide_show_preserves_normal_geometry_while_maximized -- --ignored
```

The tests must be launched as separate commands. Slint's GUI platform can only
be initialized once in a test process, so running both ignored tests together
would make the second test fail for a platform-initialization reason rather
than a geometry problem.
