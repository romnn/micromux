# Two projects, two proxy ports, one proxy

`blog/` and `shop/` are separate projects whose routes want different proxy ports: the blog sets
`proxy.port: 8481`, the shop `proxy.port: 8482`. Both are served by one shared `micromux proxy`
process, which listens on every port its sessions ask for. Each port serves only the routes of the
sessions that asked for it.

The services are Python's built-in web server, so the example needs only `python3`. Ports above
1024 need no privileges, so it runs without any setup.

Start each project in a terminal of its own:

```sh
cd examples/proxy-ports/blog && micromux
cd examples/proxy-ports/shop && micromux
```

Then open the two sites:

- <http://www.blog.localhost:8481>
- <http://www.shop.localhost:8482>

On the other port, each hostname gets a 404 page that lists the live sessions with their URLs, such
as <http://www.blog.localhost:8482>. One process listens on both ports:

```sh
lsof -nP -iTCP:8481 -iTCP:8482 -sTCP:LISTEN
```

Stop the shop, and the proxy stops listening on 8482 a minute later. It keeps 8481, the port the
blog started it on, until it exits. Stop both, and the proxy exits a minute after the last session
is gone.

Port 80 gives URLs without a port, but on Linux it needs privileges. To use it where it is allowed
and fall back elsewhere, write:

```yaml
proxy:
  port: 80
  fallback_port: 8482
```

See [the proxy](https://romnn.github.io/micromux/docs/configuration/sessions/#the-proxy) in the
documentation.
