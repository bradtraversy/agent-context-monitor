# Signs one file with Azure Artifact Signing. Tauri runs this from the
# Installers workflow, after the Azure login step.
param([Parameter(Mandatory)][string]$File)

$ErrorActionPreference = 'Stop'

Invoke-ArtifactSigning `
  -Endpoint 'https://eus.codesigning.azure.net/' `
  -CodeSigningAccountName 'bradtraversy-signing' `
  -CertificateProfileName 'bradtraversy-public' `
  -Files $File `
  -FileDigest SHA256 `
  -TimestampRfc3161 'http://timestamp.acs.microsoft.com' `
  -TimestampDigest SHA256
