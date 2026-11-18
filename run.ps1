param(
    [switch]$Release,
    [switch]$BuildOnly,
    [switch]$Test
)

$ErrorActionPreference = 'Stop'
$previousPath = $env:PATH
Push-Location -LiteralPath $PSScriptRoot
try {
    # Rust's GNU toolchain needs the MinGW linker and dlltool on PATH.
    $rustHost = (& rustc -vV | Select-String '^host:').ToString()
    if ($rustHost -match 'windows-gnu' -and -not (Get-Command dlltool.exe -ErrorAction SilentlyContinue)) {
        $mingwBin = 'C:\msys64\mingw64\bin'
        if (Test-Path -LiteralPath (Join-Path $mingwBin 'dlltool.exe')) {
            $env:PATH = $mingwBin + ';' + $env:PATH
        } else {
            throw 'GNU Rust requires MinGW-w64 (gcc.exe and dlltool.exe) on PATH. See README.md.'
        }
    }

    $cargoArguments = @('run', '--locked')
    if ($Test) { $cargoArguments[0] = 'test' }
    elseif ($BuildOnly) { $cargoArguments[0] = 'build' }
    if ($Release) { $cargoArguments += '--release' }
    & cargo @cargoArguments
    if ($LASTEXITCODE -ne 0) { throw "cargo failed with exit code $LASTEXITCODE" }
} finally {
    $env:PATH = $previousPath
    Pop-Location
}

