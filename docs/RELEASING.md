# Releasing

Bump `version` in `src-tauri/tauri.conf.json`, then push a matching tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The Installers workflow builds on all three OSes and creates a draft release with the files attached. Check it over and publish it on GitHub.

Signing turns on once these repository secrets are set (Settings, Secrets and variables, Actions). Without them the builds still work, unsigned.

- macOS: `APPLE_CERTIFICATE` (base64 Developer ID Application `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, and for notarizing `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password), `APPLE_TEAM_ID`
- Windows (Azure Artifact Signing, account `bradtraversy-signing`, profile `bradtraversy-public`): `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID`. There's no password: the Azure app trusts this repo's `release` environment through a federated credential.
