# Signs one file with Azure Artifact Signing. Tauri runs this from the
# Installers workflow, after the Azure login step.
param([Parameter(Mandatory)][string]$File)

$ErrorActionPreference = 'Stop'

# Sign in only through the Azure CLI login from the workflow. Without this the
# module can fall back to a browser sign-in, which waits forever on a runner.
$params = @{
  Endpoint                            = 'https://eus.codesigning.azure.net/'
  CodeSigningAccountName              = 'bradtraversy-signing'
  CertificateProfileName              = 'bradtraversy-public'
  Files                               = $File
  FileDigest                          = 'SHA256'
  TimestampRfc3161                    = 'http://timestamp.acs.microsoft.com'
  TimestampDigest                     = 'SHA256'
  ExcludeEnvironmentCredential        = $true
  ExcludeWorkloadIdentityCredential   = $true
  ExcludeManagedIdentityCredential    = $true
  ExcludeSharedTokenCacheCredential   = $true
  ExcludeVisualStudioCredential       = $true
  ExcludeVisualStudioCodeCredential   = $true
  ExcludeAzurePowerShellCredential    = $true
  ExcludeAzureDeveloperCliCredential  = $true
  ExcludeInteractiveBrowserCredential = $true
}

# The Azure CLI can be slow on its first token request, so fetch one up front
# and retry a couple of times before giving up.
az account get-access-token --resource https://codesigning.azure.net --output none

for ($attempt = 1; ; $attempt++) {
  try {
    Invoke-ArtifactSigning @params
    break
  } catch {
    if ($attempt -ge 3) { throw }
    Write-Warning "Signing attempt $attempt failed: $_"
    Start-Sleep -Seconds 10
  }
}
