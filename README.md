# compare-svg

Compare SVG files in a Git working tree with their versions at `HEAD`. The command generates an HTML comparison and prints its path.

## Examples

Compare one SVG:

```sh
compare-svg path/to/image.svg
```

Compare multiple SVGs from the same Git working tree:

```sh
compare-svg path/to/first.svg path/to/second.svg
```

Generate the comparison and open it in the default browser:

```sh
compare-svg --open path/to/image.svg
```
