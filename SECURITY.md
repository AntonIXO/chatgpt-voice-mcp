# Security notes

The server handles an account credential file and should be treated as a local
credential-bearing process.

- Run `chatgpt-voice-mcp login` in a trusted terminal.
- Keep `CHATGPT_VOICE_CODEX_HOME` on a local filesystem with normal Unix user
  permissions.
- Do not pass access tokens through MCP tool arguments, shell history, issue
  comments, or logs.
- `voice_auth_status` intentionally reports only paths, file permissions, and a
  boolean; it never reads credential fields into its response.
- Report a suspected credential leak privately to the repository owner instead
  of opening a public issue.
