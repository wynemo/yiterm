# Prompt wrapping approach adapted from ELYTerminal (MIT); see licenses/elyterminal-MIT.txt.
# Installed only in this new session, after the user's normal profiles have run.
# Keep the existing prompt, including Starship/Oh My Posh, and its return value.
$global:YiTermDirectoryOriginalPrompt = $function:prompt
$global:YiTermDirectoryToken = '__YITERM_DIRECTORY_TOKEN__'
function global:prompt {
    $yitermPreviousSuccess = $?
    # Restore the preceding failure status before invoking the user's prompt.
    if (-not $yitermPreviousSuccess) { Write-Error '' -ErrorAction Ignore }
    $yitermPromptText = & $global:YiTermDirectoryOriginalPrompt
    $yitermUri = 'file:///'
    $yitermSuffix = ':unavailable'
    try {
        $yitermLocation = $executionContext.SessionState.Path.CurrentLocation
        if ($yitermLocation.Provider.Name -eq 'FileSystem') {
            $yitermFileUri = [System.Uri]($yitermLocation.ProviderPath)
            if ($yitermFileUri.IsAbsoluteUri -and $yitermFileUri.IsFile) {
                $yitermUri = $yitermFileUri.AbsoluteUri
                $yitermSuffix = ''
            }
        }
    } catch { }
    try {
        # The fragment is session-local metadata, never part of the file path.
        # Do not export the token to child programs (in particular ssh).
        [Console]::Write(('{0}]7;{1}#yiterm-cwd={2}{3}{4}' -f [char]27, $yitermUri, $global:YiTermDirectoryToken, $yitermSuffix, [char]7))
    } catch { }
    $yitermPromptText
}
