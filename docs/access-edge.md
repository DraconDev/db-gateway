# Non-loopback access edge — decision and threat model (audit A9)

Status: **RATIFIED by the owner 2026-10-19** — approval record: the
owner selected option 1 ("Ratify the mesh edge") of this goal's GLLA
decision pause, adopting WireGuard + loopback socket proxies as the
canonical application access story, with installs explicitly a
separate future approval. Nothing in this document is installed
anywhere.
The gateway today is loopback-only plus SSH tunnel (readiness gate 3,
accepted for the owner CLI). This document decides how *application*
hosts reach it before the Turso cutover (audit A6) needs them to.

## Decision

**Application hosts reach the gateway over a WireGuard mesh. On the
gateway host, a systemd socket-proxy bound to the host's WireGuard
address forwards to the gateway on loopback. The gateway binary does
not change: it keeps refusing every non-loopback bind.**

Concretely, per deployment (templates `deploy/dg-mesh-proxy.socket` +
`deploy/dg-mesh-proxy.service`, non-installing):

1. Gateway host and each application host join one WireGuard network
   (owner-operated; keys exchanged out of band).
2. `dg-mesh-proxy.socket` binds `{{DG_MESH_LISTEN}}` = the gateway
   host's WireGuard `address:port` (e.g. `10.66.0.1:7432`) — never a
   wildcard, LAN, or WAN address.
3. `systemd-socket-proxyd` forwards to `{{DG_LISTEN}}`
   (127.0.0.1:7432) with a connection cap.
4. Each application host runs the mirror-image forwarder
   (`deploy/dg-app-proxy.{socket,service}` templates): a socket on
   the app host's own `127.0.0.1:7432`, proxied through the mesh to
   the gateway's `{{DG_MESH_LISTEN}}`. Applications therefore keep
   using `http://127.0.0.1:7432` origins with `dg-client` / the TS
   client exactly as the owner CLI does today — **no client origin
   rule changes anywhere.**

   Rationale for keeping the client rule untouched: "plain HTTP
   only ever on literal loopback" is enforced identically in the
   server, the CLI, and both clients. Weakening it to "loopback or
   mesh/RFC1918" would have to be argued four times and would
   silently bless LAN plaintext too. Loopback forwarders at both
   ends keep plaintext on the host it terminates on; everything
   that crosses a wire is WireGuard-encrypted.

5. SSH tunnels remain the owner-CLI path and the interim application
   path until a given host joins the mesh; both coexist because both
   terminate at loopback.

## Threat model

Assets: database contents, scoped credentials (64-hex, hash-only at
rest), owner credential, availability of the single writer.

| Threat | Mitigation in this design | Residual |
|---|---|---|
| Network eavesdropper / MITM between app host and gateway | WireGuard (Noise, per-host keys) encrypts and authenticates the transport; plaintext exists only on loopback segments at each end | A compromised endpoint sees its own plaintext — unavoidable in any design without in-app TLS, accepted |
| Internet/LAN attacker reaching the dg port | Nothing listens off-loopback and off-mesh: `dg serve` refuses non-loopback binds (tested, `invalid_listen_address`), the proxy binds only the mesh address | Host firewall hygiene still applies; a mis-set `{{DG_MESH_LISTEN}}` (wildcard) would break this — the template says so in capitals, and install review must check it |
| Credential guessing over the mesh | 64-hex keys (256-bit), uniform `401 invalid_credential`, cheap rejection; negative-tested: 64 bad keys all rejected, legitimate scoped traffic immediately unaffected (`tests/access_edge.rs`) | No per-source auth throttle in dg by design — guessing is infeasible; a mesh member flooding *valid-shape* requests is bounded by admission lanes (8 read / 4 write) and the proxy connection cap |
| Compromised application host | Scoped, generation-bound keys: that host's key reaches only its databases; revocation is immediate and tested; owner key never leaves the gateway host | The compromised app can abuse its own databases' data — inherent to giving it a key at all; detection via status/digest, response via revoke |
| Compromised mesh member (not an app host) | Same as network attacker post-mesh: still needs a dg key; mesh membership alone grants nothing but reachability | If the mesh ever carries members the owner does not fully trust, this edge is wrong — revisit toward per-app TLS (rejected-for-now below) |
| Availability attack via the proxy | `systemd-socket-proxyd --max-connections=32`, admission lanes + 429/408 backpressure behind it | A determined mesh member can still consume the 32 proxy slots; alerting on dg.service health is the detector, not a prevention |

## Rejected alternatives

- **dg binds a private/mesh address directly.** Breaks the
  by-construction rule (one bind, loopback, refused otherwise) that
  four test suites lean on, and puts plaintext HTTP on an interface
  whose encryption is then a configuration property, not a
  construction property. Rejected.
- **Public TLS terminator (Caddy/nginx on 443).** The only option
  that creates Internet exposure: certificate lifecycle, a public
  attack surface, and a real need for the auth throttling dg does
  not have. Disproportionate for a single-owner estate whose app
  hosts can join a mesh. Rejected for now; it is the named fallback
  if untrusted members ever need access.
- **SSH tunnel per application host as the permanent edge.** Works
  today and stays as the interim path, but as the permanent story it
  spreads SSH credentials to every app host and makes tunnel
  babysitting each app's problem. The mesh centralizes host identity
  in WireGuard keys. Superseded, not removed.
- **In-gateway TLS.** A second TLS stack inside dg duplicates what
  WireGuard/the terminator already do and expands the audited
  surface of the data path. Rejected (consistent with readiness
  gate 3: no TLS in the gateway).

## What "plain-HTTP impossible" means after this decision

Plaintext HTTP exists in exactly two places, both loopback: app →
its own host's forwarder, and proxy → dg on the gateway host. Every
wire crossing is WireGuard ciphertext. Making plaintext appear on a
wire requires changing dg's bind rule and three client origin rules
— four deliberate, tested code changes, not a configuration slip.

## Open items after ratification

- Owner picks the mesh addressing and which hosts join first
  (suggestion: billing-api's host first, per the A6 pilot order).
- Install (socket-proxy units + WireGuard itself) is a separate
  owner-approved live change; this document authorizes nothing.
- If a future app host cannot run WireGuard or a forwarder, it does
  not get access under this edge — that constraint is the decision,
  not an oversight.
