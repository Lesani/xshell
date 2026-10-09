# The Relay is one Cloudflare Worker codebase, also shipped on workerd

The Relay (Hosted or self-hosted) is a TypeScript Worker with one Durable Object per Ring using WebSocket Hibernation, so an idle Ring costs nothing on Cloudflare's free tier. Self-hosters who do not use Cloudflare run the same code in a Docker image on `workerd`, Cloudflare's open-source runtime, behind their own TLS or tunnel. The Hosted Relay is the same code with the subscription check on. Desktops default to the Hosted Relay with nothing to type; self-hosters replace the URL, which is carried in the signed Roster so every device of a Ring agrees on it.

## Considered Options

- **A Rust relay binary for Docker next to the Worker.** Rejected: two implementations of routing, Roster storage and quotas.
- **A Desktop wizard that deploys to the user's Cloudflare account with an API token.** Rejected: xshell would handle Cloudflare credentials; a "Deploy to Cloudflare" button gives nearly the same convenience.
