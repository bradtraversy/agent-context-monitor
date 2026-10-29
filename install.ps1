# Installs the latest Agent Context Monitor release on Windows.
#
#   irm https://raw.githubusercontent.com/bradtraversy/agent-context-monitor/main/install.ps1 | iex
#
# Set $env:ACM_VERSION = "v0.1.1" first to install a specific release instead of the latest.

# Runs in its own scope so these settings don't leak into your PowerShell session.
& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'

    $repo = 'bradtraversy/agent-context-monitor'
    $appName = 'Agent Context Monitor'

    # Windows PowerShell 5.1 may default to an old TLS version that GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    if ($env:ACM_VERSION) {
        $api = "https://api.github.com/repos/$repo/releases/tags/$env:ACM_VERSION"
    } else {
        $api = "https://api.github.com/repos/$repo/releases/latest"
    }

    Write-Host 'Checking for the latest release...'
    try {
        $headers = @{ Accept = 'application/vnd.github+json' }
        # A token is optional. It only raises GitHub's rate limit, which CI runners share.
        if ($env:ACM_GITHUB_TOKEN) { $headers.Authorization = "Bearer $env:ACM_GITHUB_TOKEN" }
        $release = Invoke-RestMethod -Uri $api -Headers $headers
    } catch {
        $status = if ($_.Exception.Response) { [int]$_.Exception.Response.StatusCode } else { 0 }
        if ($status -eq 404 -and $env:ACM_VERSION) { throw "There's no release called $env:ACM_VERSION." }
        if ($status -eq 404) { throw "There's no published release yet." }
        if ($status -eq 403 -or $status -eq 429) { throw "GitHub's rate limit was hit. Wait a few minutes and try again." }
        if ($status) { throw "GitHub answered with HTTP $status. Try again in a moment." }
        throw "Couldn't reach GitHub. Check your connection and try again."
    }

    $asset = $release.assets | Where-Object { $_.name -like '*_x64-setup.exe' } | Select-Object -First 1
    if (-not $asset) { throw "No Windows installer found in release $($release.tag_name)." }

    $installer = Join-Path ([IO.Path]::GetTempPath()) $asset.name
    Write-Host "Downloading $($asset.name)..."
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $installer -UseBasicParsing

    try {
        # The installer is signed. Refuse to run it if the signature doesn't check out.
        $sig = Get-AuthenticodeSignature $installer
        if ($sig.Status -ne 'Valid') {
            throw "The downloaded installer isn't validly signed ($($sig.Status)), so it wasn't run."
        }

        # Quit a running copy so it can be replaced.
        Get-Process -Name 'agent-context-monitor' -ErrorAction SilentlyContinue | Stop-Process -Force
        Start-Sleep -Milliseconds 500

        Write-Host 'Installing...'
        $proc = Start-Process -FilePath $installer -ArgumentList '/S' -Wait -PassThru
        if ($proc.ExitCode -ne 0) { throw "The installer exited with code $($proc.ExitCode)." }
    } finally {
        Remove-Item $installer -Force -ErrorAction SilentlyContinue
    }

    $exe = Join-Path $env:LOCALAPPDATA "$appName\agent-context-monitor.exe"
    if (-not (Test-Path $exe)) { $exe = Join-Path $env:ProgramFiles "$appName\agent-context-monitor.exe" }

    Write-Host ''
    if (Test-Path $exe) {
        Start-Process -FilePath $exe
        Write-Host "$appName $($release.tag_name) is installed and running. Look for it in the system tray."
    } else {
        Write-Host "$appName $($release.tag_name) is installed. Open it from the Start menu."
    }
}
