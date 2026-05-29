function Get-AiMemoryCwd {
    param([string] $Payload)
    if (-not $Payload) { return $null }
    try {
        $Parsed = $Payload | ConvertFrom-Json -ErrorAction Stop
        foreach ($Name in @("cwd", "current_dir", "working_dir", "directory")) {
            $Value = $Parsed.$Name
            if ($Value -is [string] -and $Value.Length -gt 0) { return $Value }
        }
        $Paths = $Parsed.workspacePaths
        if ($null -ne $Paths -and $Paths.Count -gt 0 -and $Paths[0] -is [string] -and $Paths[0].Length -gt 0) {
            return $Paths[0]
        }
    } catch {
    }
    $match = [regex]::Match($Payload, '"cwd"\s*:\s*"([^"]*)"')
    if ($match.Success) { return $match.Groups[1].Value }
    $workspaceMatch = [regex]::Match($Payload, '"workspacePaths"\s*:\s*\[\s*"([^"]*)"')
    if ($workspaceMatch.Success) { return $workspaceMatch.Groups[1].Value }
    return $null
}

function Get-AiMemoryMarkerToml {
    param([string] $Cwd)
    if (-not $Cwd) { return $null }
    $dir = $Cwd
    while ($dir -and (Test-Path $dir)) {
        $candidate = Join-Path $dir ".ai-memory.toml"
        if (Test-Path $candidate -PathType Leaf) { return $candidate }
        if ($env:HOME -and $dir -eq $env:HOME) { return $null }
        if ($env:USERPROFILE -and $dir -eq $env:USERPROFILE) { return $null }
        $parent = Split-Path $dir -Parent
        if (-not $parent -or $parent -eq $dir) { return $null }
        $dir = $parent
    }
    return $null
}

function Get-AiMemoryTomlKey {
    param([string] $File, [string] $Key)
    if (-not (Test-Path $File -PathType Leaf)) { return $null }
    foreach ($line in Get-Content $File) {
        $m = [regex]::Match($line, "^\s*$Key\s*=\s*`"([^`"]*)`"")
        if ($m.Success) { return $m.Groups[1].Value }
    }
    return $null
}

function Get-AiMemoryMarkerQuery {
    param([string] $Cwd)
    if (-not $Cwd) { return "" }
    $qs = "&cwd=$([uri]::EscapeDataString($Cwd))"
    $marker = Get-AiMemoryMarkerToml -Cwd $Cwd
    if (-not $marker) { return $qs }
    $ws = Get-AiMemoryTomlKey -File $marker -Key "workspace"
    if ($ws) { $qs += "&workspace=$([uri]::EscapeDataString($ws))" }
    $proj = Get-AiMemoryTomlKey -File $marker -Key "project"
    if ($proj) { $qs += "&project=$([uri]::EscapeDataString($proj))" }
    $strategy = Get-AiMemoryTomlKey -File $marker -Key "project_strategy"
    if ($strategy) { $qs += "&project_strategy=$([uri]::EscapeDataString($strategy))" }
    return $qs
}

function Read-AiMemoryStdin {
    try {
        if (-not [Console]::IsInputRedirected) { return "" }
        $StdinStream = [Console]::OpenStandardInput()
        $StdinReader = [System.IO.StreamReader]::new($StdinStream, [System.Text.Encoding]::UTF8, $false, 4096)
        $ReadTask = $StdinReader.ReadToEndAsync()
        if ($ReadTask.Wait(2000)) {
            $result = $ReadTask.Result
            $StdinReader.Dispose()
            $StdinStream.Dispose()
            return $result
        }
        $StdinReader.Dispose()
        $StdinStream.Dispose()
    } catch {
    }
    return ""
}

function Invoke-AiMemoryHook {
    param(
        [Parameter(Mandatory = $true)] [string] $Event,
        [Parameter(Mandatory = $true)] [string] $Agent,
        [switch] $FetchHandoff,
        [switch] $AntigravityPreInvocationOutput
    )

    $Server = if ($env:AI_MEMORY_HOOK_URL) { $env:AI_MEMORY_HOOK_URL } else { "http://127.0.0.1:49374" }
    $Payload = Read-AiMemoryStdin
    $Cwd = Get-AiMemoryCwd -Payload $Payload
    $QS = Get-AiMemoryMarkerQuery -Cwd $Cwd
    $Headers = @{}

    if ($env:AI_MEMORY_AUTH_TOKEN) {
        $Headers["Authorization"] = "Bearer $env:AI_MEMORY_AUTH_TOKEN"
    }

    try {
        Invoke-WebRequest `
            -UseBasicParsing `
            -TimeoutSec 3 `
            -Method Post `
            -Uri "$Server/hook?event=$Event&agent=$Agent$QS" `
            -Headers $Headers `
            -ContentType "application/json" `
            -Body $Payload | Out-Null
    } catch {
    }

    if ($FetchHandoff) {
        try {
            $Response = Invoke-WebRequest `
                -UseBasicParsing `
                -TimeoutSec 2 `
                -Uri "$Server/handoff?agent=$Agent$QS" `
                -Headers $Headers
            if ($null -ne $Response -and $Response.Content) {
                if ($AntigravityPreInvocationOutput) {
                    $Payload = @{
                        injectSteps = @(@{ ephemeralMessage = $Response.Content })
                    }
                    [Console]::Out.Write(($Payload | ConvertTo-Json -Depth 5 -Compress))
                } else {
                    [Console]::Out.Write($Response.Content)
                }
            } elseif ($AntigravityPreInvocationOutput) {
                [Console]::Out.Write("{}")
            }
        } catch {
            if ($AntigravityPreInvocationOutput) {
                [Console]::Out.Write("{}")
            }
        }
    } elseif ($AntigravityPreInvocationOutput) {
        [Console]::Out.Write("{}")
    }
}
