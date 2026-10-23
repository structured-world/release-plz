# Website

This website is built using [Docusaurus 3](https://docusaurus.io/).

## Local Development

```sh
npm ci
npm start
```

This command starts a local development server and opens up a browser window. Most changes are
reflected live without having to restart the server.

Use Node.js 24, as in CI. The dependency tree requires Node.js 22.12 or newer.

## Dependency security

The package overrides select patched transitive dependencies while their consumers
still request vulnerable versions. They cover Tinypool's worker options,
serialize-javascript's serialization, PostCSS selector parsing, the Markdown
linters' YAML/TOML/math parsers, and SockJS's UUID generation. Keep the overrides
until the corresponding consumers require patched versions themselves.

After changing them, run `npm ci`, `npm audit`, `npm run format:check`,
`npm run mdlint`, and `npm run build`. Also check the development server and the
Docusaurus threaded static-generation path when changing Tinypool.
