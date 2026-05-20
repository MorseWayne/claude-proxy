$ErrorActionPreference = "Stop"

$Repo = "MorseWayne/claude-proxy"
$Target = "x86_64-pc-windows-msvc"
$InstallDir = if ($env:CLP_INSTALL_DIR) {
    $env:CLP_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "Programs\claude-proxy"
}
$ServiceWasRunning = $false

function Test-Architecture {
    $arch = $env:PROCESSOR_ARCHITEW6432
    if ([string]::IsNullOrWhiteSpace($arch)) {
        $arch = $env:PROCESSOR_ARCHITECTURE
    }

    if ($arch -ne "AMD64") {
        throw "Unsupported Windows architecture: $arch. Download manually from GitHub Releases."
    }
}

function Add-ToUserPath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$PathToAdd
    )

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $pathParts = @()
    if (-not [string]::IsNullOrWhiteSpace($userPath)) {
        $pathParts = $userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    }

    $alreadyInPath = $pathParts | Where-Object { $_.TrimEnd('\') -ieq $PathToAdd.TrimEnd('\') }
    if (-not $alreadyInPath) {
        $newPath = if ([string]::IsNullOrWhiteSpace($userPath)) {
            $PathToAdd
        } else {
            "$userPath;$PathToAdd"
        }

        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Host "Added $PathToAdd to your user PATH."
        Write-Host "Restart your terminal to use claude-proxy globally."
        Write-Host ""
    }
}

function Get-ClaudeProxyProcesses {
    @(Get-Process -Name "claude-proxy" -ErrorAction SilentlyContinue)
}

function Confirm-Continue {
    $answer = Read-Host "Continue? [y/N]"
    return $answer -in @("y", "Y", "yes", "YES", "Yes")
}

function Prepare-ExistingService {
    if ((Get-ClaudeProxyProcesses).Count -eq 0) {
        return
    }

    Write-Host "A running claude-proxy process was detected."
    Write-Host "The installer will stop it before replacing the binary and restart it afterward."
    if (-not (Confirm-Continue)) {
        Write-Host "Installation cancelled."
        exit 1
    }

    $script:ServiceWasRunning = $true
}

function Stop-ExistingService {
    if (-not $script:ServiceWasRunning) {
        return
    }

    $processes = Get-ClaudeProxyProcesses
    if ($processes.Count -eq 0) {
        Write-Host "claude-proxy is no longer running."
        return
    }

    Write-Host "Stopping existing claude-proxy process..."
    $processes | Stop-Process -Force -ErrorAction Stop

    foreach ($process in $processes) {
        Wait-Process -Id $process.Id -Timeout 10 -ErrorAction SilentlyContinue
        if (Get-Process -Id $process.Id -ErrorAction SilentlyContinue) {
            throw "Failed to stop claude-proxy process $($process.Id) within 10 seconds."
        }
    }
}

function Restart-ExistingService {
    param(
        [Parameter(Mandatory = $true)]
        [string]$BinaryPath
    )

    if (-not $script:ServiceWasRunning) {
        return
    }

    Write-Host "Restarting claude-proxy..."
    Start-Process -FilePath $BinaryPath -ArgumentList @("server", "start") -WindowStyle Hidden | Out-Null
}

function Install-ClaudeProxy {
    Test-Architecture

    $archive = "claude-proxy-$Target.zip"
    $url = "https://github.com/$Repo/releases/latest/download/$archive"
    $tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())

    Prepare-ExistingService

    Write-Host "Downloading claude-proxy for Windows..."
    Write-Host "  URL: $url"

    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

    try {
        $archivePath = Join-Path $tempDir $archive
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archivePath
        Expand-Archive -Path $archivePath -DestinationPath $tempDir -Force

        Stop-ExistingService

        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        $binaryPath = Join-Path $InstallDir "claude-proxy.exe"
        Move-Item -Force (Join-Path $tempDir "claude-proxy.exe") $binaryPath

        Restart-ExistingService -BinaryPath $binaryPath

        Write-Host ""
        Write-Host "Installed to $binaryPath"
        Write-Host ""

        Add-ToUserPath -PathToAdd $InstallDir

        & $binaryPath --version
    } finally {
        if (Test-Path $tempDir) {
            Remove-Item -Recurse -Force $tempDir
        }
    }
}

Install-ClaudeProxy
