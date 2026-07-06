# React SSR on lumen

Server-side renders a React component tree to HTML with `react-dom/server`'s `renderToString`,
served over `node:http`. Uses `React.createElement` directly, so there's no JSX build step.

```sh
npm install
../../target/release/lumen-cli server.js   # http://localhost:3000
```
