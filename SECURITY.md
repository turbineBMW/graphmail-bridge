# Security

`graphmail-bridge` handles OAuth refresh tokens and can read, send, move, and
delete mail. Version 0.1 is experimental; use it with a non-critical mailbox
first.

- The IMAP and SMTP listeners are restricted to loopback addresses and do not
  offer TLS. Never proxy or forward these ports to another host.
- The generated bridge password protects against other local clients but is not
  a substitute for operating-system account isolation.
- OAuth credentials use the desktop keyring by default. The optional file
  backend is plaintext JSON protected only by Unix mode 0600.
- The bridge never needs an Entra client secret. Do not put one in its config.
- The default compatibility profile presents Microsoft's public Office client
  identity to Entra. Audit logs therefore name Microsoft Office, not
  graphmail-bridge. Use `custom-entra` when accurate application attribution is
  required by your organization.
- Neither authentication profile bypasses Conditional Access or device
  compliance. Do not weaken or evade those controls to deploy the bridge.
- Logs avoid tokens, passwords, and message bodies. Graph errors can contain
  request identifiers and Microsoft diagnostic text.

When reporting a vulnerability, do not include real tokens, message content,
tenant IDs, or email addresses in a public issue.
