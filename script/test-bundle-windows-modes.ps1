# Run in Windows CI with: pwsh -NoProfile -File script/test-bundle-windows-modes.ps1
$ErrorActionPreference = 'Stop'

$bundlePath = Join-Path $PSScriptRoot 'bundle-windows.ps1'
$tokens = $null
$parseErrors = $null
$bundle = [System.Management.Automation.Language.Parser]::ParseFile($bundlePath, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) {
    throw ($parseErrors | Out-String)
}

# Load definitions without running Visual Studio, Cargo, installers, or downloads.
foreach ($definition in $bundle.EndBlock.Statements | Where-Object { $_ -is [System.Management.Automation.Language.FunctionDefinitionAst] }) {
    . ([scriptblock]::Create($definition.Extent.Text))
}
$modeAssignments = $bundle.EndBlock.Statements | Where-Object {
    $_ -is [System.Management.Automation.Language.AssignmentStatementAst] -and
    $_.Left.Extent.Text -in @('$buildDesktop', '$buildRemoteServer')
}
# ParamBlock's extent starts at 'param'; its preceding attributes have separate extents.
$parameterAttributes = ($bundle.ParamBlock.Attributes | ForEach-Object { $_.Extent.Text }) -join "`n"
$bindMode = [scriptblock]::Create($parameterAttributes + "`n" + $bundle.ParamBlock.Extent.Text + "`n" +
    (($modeAssignments | ForEach-Object { $_.Extent.Text }) -join "`n") + "`n" +
    '[pscustomobject]@{ Desktop = $buildDesktop; Remote = $buildRemoteServer }')

function AssertEqual($Actual, $Expected, [string]$Message) {
    if (($Actual -join "`n") -cne ($Expected -join "`n")) {
        throw "$Message. Expected: [$Expected]; actual: [$Actual]"
    }
}

$environmentNames = @(
    'CI', 'ZED_WORKSPACE', 'RELEASE_VERSION', 'ZED_RELEASE_CHANNEL', 'RELEASE_CHANNEL',
    'SENTRY_AUTH_TOKEN', 'ENDPOINT', 'ACCOUNT_NAME', 'CERT_PROFILE_NAME', 'FILE_DIGEST',
    'TIMESTAMP_DIGEST', 'TIMESTAMP_SERVER', 'TRACE'
)
$savedEnvironment = @{}
foreach ($name in $environmentNames) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name)
}

try {
    $env:CI = 'true'
    foreach ($case in @(
        @{ Arguments = @{}; Desktop = $true; Remote = $true },
        @{ Arguments = @{ DesktopOnly = $true }; Desktop = $true; Remote = $false },
        @{ Arguments = @{ RemoteServerOnly = $true }; Desktop = $false; Remote = $true }
    )) {
        & {
            $arguments = $case.Arguments
            $mode = & $bindMode @arguments
            AssertEqual $mode.Desktop $case.Desktop 'Desktop mode selection'
            AssertEqual $mode.Remote $case.Remote 'Remote mode selection'
            $buildDesktop = $mode.Desktop
            $buildRemoteServer = $mode.Remote
            $script:buildSuccess = $false
            $steps = [System.Collections.Generic.List[string]]::new()
            $pipelineSteps = @(
                'CheckEnvironmentVariables', 'PrepareForBundle', 'GenerateLicenses',
                'BuildZedAndItsFriends', 'BuildRemoteServer', 'MakeAppx', 'SignZedAndItsFriends',
                'ZipZedAndItsFriendsDebug', 'DownloadAMDGpuServices', 'DownloadConpty',
                'CollectFiles', 'StageLinuxRemoteServer', 'BuildInstaller', 'UploadToSentry'
            )
            foreach ($step in $pipelineSteps) {
                Set-Item "Function:$step" ([scriptblock]::Create("`$steps.Add('$step')"))
            }
            InvokeBundlePipeline
            $expected = @('CheckEnvironmentVariables', 'PrepareForBundle')
            if ($case.Desktop) { $expected += @('GenerateLicenses', 'BuildZedAndItsFriends') }
            if ($case.Remote) { $expected += 'BuildRemoteServer' }
            if ($case.Desktop) { $expected += @('MakeAppx', 'SignZedAndItsFriends') }
            $expected += 'ZipZedAndItsFriendsDebug'
            if ($case.Desktop) {
                $expected += @('DownloadAMDGpuServices', 'DownloadConpty', 'CollectFiles', 'StageLinuxRemoteServer', 'BuildInstaller')
            } else {
                AssertEqual $script:buildSuccess $true 'Remote-only completes without an installer'
            }
            $expected += 'UploadToSentry'
            AssertEqual $steps $expected 'Mode pipeline and ordering'
        }

        & {
            $buildDesktop = $case.Desktop
            $buildRemoteServer = $case.Remote
            $CargoOutDir = './target/x86_64-pc-windows-msvc/release'
            $debugArchive = 'selected-mode.dbg.zip'
            $expectedSymbols = @()
            if ($case.Desktop) {
                $expectedSymbols += @('zed', 'cli', 'auto_update_helper', 'explorer_command_injector') |
                    ForEach-Object { ".\$CargoOutDir\$_.pdb" }
            }
            if ($case.Remote) { $expectedSymbols += ".\$CargoOutDir\remote_server.pdb" }
            function Compress-Archive($Path, $DestinationPath, [switch]$Force) {
                AssertEqual $Path $expectedSymbols 'Debug ZIP contains only selected symbols'
                AssertEqual $DestinationPath $debugArchive 'Debug ZIP destination'
            }
            ZipZedAndItsFriendsDebug
            $env:SENTRY_AUTH_TOKEN = 'test-token'
            function Get-Command { return $true }
            function sentry-cli {
                AssertEqual $args (@('debug-files', 'upload', '--include-sources', '--wait', '-p', 'zed', '-o', 'zed-dev') + $expectedSymbols) 'Sentry uploads only selected symbols, not the cached target directory'
            }
            UploadToSentry
        }
    }

    foreach ($arguments in @(
        @{ DesktopOnly = $true; RemoteServerOnly = $true },
        @{ RemoteServerOnly = $true; Install = $true }
    )) {
        $rejected = $false
        try { & $bindMode @arguments }
        catch [System.Management.Automation.ParameterBindingException] { $rejected = $true }
        AssertEqual $rejected $true 'Incompatible switches are rejected before setup'
    }

    & {
        $buildDesktop = $false
        $target = 'x86_64-pc-windows-msvc'
        $commands = [System.Collections.Generic.List[string]]::new()
        function rustup { $commands.Add($args -join ' ') }
        function Test-Path { throw 'Remote-only must not inspect Inno staging' }
        function New-Item { throw 'Remote-only must not create installer staging' }
        function Copy-Item { throw 'Remote-only must not copy installer resources' }
        PrepareForBundle
        AssertEqual $commands @('target add x86_64-pc-windows-msvc') 'Remote-only preparation'
    }

    foreach ($signed in @($false, $true)) {
        & {
            $canCodeSign = $signed
            $Architecture = 'x86_64'
            $target = 'x86_64-pc-windows-msvc'
            $CargoProfile = 'release'
            $CargoOutDir = "./target/$target/$CargoProfile"
            $env:ZED_WORKSPACE = Split-Path $PSScriptRoot -Parent
            foreach ($name in @('ENDPOINT', 'ACCOUNT_NAME', 'CERT_PROFILE_NAME', 'FILE_DIGEST', 'TIMESTAMP_DIGEST', 'TIMESTAMP_SERVER')) {
                [Environment]::SetEnvironmentVariable($name, 'test-value')
            }
            $env:TRACE = ''
            $operations = [System.Collections.Generic.List[string]]::new()
            function cargo {
                AssertEqual $args @('--config', '.cargo/bundle-config.toml', 'build', '--profile', 'release', '--package', 'remote_server', '--target', $target) 'Remote uses the existing release build configuration'
                $operations.Add('build')
            }
            function Resolve-Path($Path) {
                AssertEqual $Path ".\$CargoOutDir\remote_server.exe" 'Remote executable source'
                [pscustomobject]@{ Path = 'remote_server.exe' }
            }
            function Invoke-TrustedSigning {
                param($Files, $Endpoint, $CodeSigningAccountName, $CertificateProfileName, $FileDigest, $TimestampDigest, $TimestampRfc3161)
                AssertEqual $Files 'remote_server.exe' 'Sign only the remote executable'
                $operations.Add('sign')
            }
            function Compress-Archive($Path, $DestinationPath, [switch]$Force) {
                AssertEqual $Path 'remote_server.exe' 'Standalone ZIP contains the executable only'
                AssertEqual $DestinationPath "$env:ZED_WORKSPACE\target\zed-remote-server-windows-x86_64.zip" 'Standalone ZIP keeps the published filename'
                $operations.Add('zip')
            }
            BuildRemoteServer
            $expected = @('build')
            if ($signed) { $expected += 'sign' }
            $expected += 'zip'
            AssertEqual $operations $expected 'Remote signing occurs before packaging without Inno staging'
        }
    }

    & {
        function ParseZedWorkspace {
            $env:ZED_WORKSPACE = 'metadata-workspace'
            $env:RELEASE_VERSION = '1.2.3'
        }
        function Get-Content { return 'dev' }
        foreach ($workspace in @('', 'workflow-workspace')) {
            foreach ($version in @('', '9.8.7')) {
                $env:ZED_WORKSPACE = $workspace
                $env:RELEASE_VERSION = $version
                $env:ZED_RELEASE_CHANNEL = ''
                $env:RELEASE_CHANNEL = ''
                InitializeBundleEnvironment
                $expectedWorkspace = if ($workspace) { $workspace } else { 'metadata-workspace' }
                $expectedVersion = if ($version) { $version } else { '1.2.3' }
                AssertEqual $env:ZED_WORKSPACE $expectedWorkspace 'Workspace default preserves overrides'
                AssertEqual $env:RELEASE_VERSION $expectedVersion 'Version default preserves overrides'
                AssertEqual $env:ZED_RELEASE_CHANNEL 'dev' 'Channel defaults to the checkout'
                AssertEqual $env:RELEASE_CHANNEL 'dev' 'Channel variables agree'
            }
        }
        function ParseZedWorkspace { throw 'Metadata is unnecessary when both values are supplied' }
        $env:ZED_RELEASE_CHANNEL = ''
        $env:RELEASE_CHANNEL = 'preview'
        InitializeBundleEnvironment
        AssertEqual $env:ZED_RELEASE_CHANNEL 'preview' 'RELEASE_CHANNEL override is honored'
        $env:ZED_RELEASE_CHANNEL = 'stable'
        InitializeBundleEnvironment
        AssertEqual $env:RELEASE_CHANNEL 'stable' 'ZED_RELEASE_CHANNEL takes precedence'
    }

    Write-Output 'Windows bundle mode tests passed.'
} finally {
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name])
    }
}
