# compare-svg

Compare SVG files in a Git working tree with their versions at `HEAD`. The command starts a local web server, opens the comparison in the default browser, and returns immediately.

The server remains available while a browser is connected. After the last browser disconnects, it shuts down following a 30-second grace period.

Press Enter while viewing a snapshot to squash that file into the first commit shown by `but status`, then advance to the next snapshot.

## Examples

Compare one SVG:

```sh
compare-svg path/to/image.svg
```

Compare multiple SVGs from the same Git working tree:

```sh
compare-svg path/to/first.svg path/to/second.svg
```

Run as if the command was started in another directory:

```sh
compare-svg -C /path/to/repository path/to/image.svg
```

`-C` is global, so it also works with `serve`:

```sh
compare-svg serve -C /path/to/repository path/to/image.svg
```

Run the server in the foreground with logging:

```sh
compare-svg serve path/to/image.svg
```

Change the shutdown grace period for local testing:

```sh
compare-svg serve --timeout 300 path/to/image.svg
```

Set `RUST_LOG` to configure foreground server logging:

```sh
RUST_LOG=debug compare-svg serve path/to/image.svg
```
