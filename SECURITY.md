# Security

Hackerman passes keyboard and mouse input between computers, so a flaw in it could be serious.

## Reporting a problem

Please don't open a public issue. Use **Report a vulnerability** on the
[Security tab](https://github.com/medAmarFilali/hackerman/security) of this repository, which only
the maintainers can read. Include the version, what an attacker could do, and how to reproduce it.

Hackerman is maintained in spare time, so a first answer may take a few days. Fixes go into the
latest version only.

## How it's meant to work

- **Pairing.** Two computers pair with a 6-digit code shown on one and typed on the other, using
  SPAKE2, so someone watching the network learns nothing they could use to guess it. A wrong code
  ends the attempt.
- **Connections.** All traffic between computers is QUIC with mutual TLS. Each computer has its own
  certificate, and only certificates pinned during pairing are accepted.
- **No server.** Nothing goes through a server, and Hackerman contacts nothing outside your local
  network.
- **The local channel.** The app talks to its background agent over a named pipe that only the
  current user and the system can open.

## What a paired computer can do

Pairing is a real decision. A paired computer can type and click on this one as if it were sitting
at it, whatever else is switched off. On top of that:

- It can read files you copied, and only those, and offer to save files here, which asks first by
  default.
- It sees the names of the apps open here, and can control what's playing and bring up and move
  apps, unless you turn those off under **Settings > Apps & media**.
- It can see what's on the screens only if you turn that on for that computer under
  **Settings > Screens**, which is off by default.
- It can reach the lock and sign-in screens only if you turned that feature on, and approved that
  computer with administrator rights.
- It can reach programs running as administrator only if you turned that same feature on. Then
  programs running on your account here could use Hackerman's helper too, to type into those
  programs and to see what's typed into them. If Windows is set to show its "allow this app to
  make changes" prompts on the ordinary desktop, rather than the dimmed secure one it uses by
  default, they could answer those too. Windows doesn't treat the line between your programs and
  administrator programs as a security boundary, and other programs that share a keyboard and
  mouse across PCs have the same trade-off.

Only pair computers you trust, and forget one you no longer do.

## Downloads

Release builds aren't code-signed yet. Each one is built by
[GitHub Actions](.github/workflows/release.yml) from the tagged source, and comes with checksums and
a signed record of how it was built. The [README](README.md#checking-a-download) says how to check
them.
