$ErrorActionPreference = "Stop"

$Repo = "MorseWayne/claude-proxy"
$Target = "x86_64-pc-windows-msvc"
$InstallDir = if ($env:CLP_INSTALL_DIR) {
    $env:CLP_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "Programs\claude-proxy"
}

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

function Install-ClaudeProxy {
    Test-Architecture

    $archive = "claude-proxy-$Target.zip"
    $url = "https://github.com/$Repo/releases/latest/download/$archive"
    $tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())

    Write-Host "Downloading claude-proxy for Windows..."
    Write-Host "  URL: $url"

    New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

    try {
        $archivePath = Join-Path $tempDir $archive
        Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archivePath
        Expand-Archive -Path $archivePath -DestinationPath $tempDir -Force

        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        $binaryPath = Join-Path $InstallDir "claude-proxy.exe"
        Move-Item -Force (Join-Path $tempDir "claude-proxy.exe") $binaryPath

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
