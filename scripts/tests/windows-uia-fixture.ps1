param(
    [Parameter(Mandatory = $true)][string]$ArtifactDirectory
)

# Deliberately interactive test fixture. It never opens a business document or
# sends input to an existing application. Launch only for an announced live test.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName PresentationFramework
Add-Type -AssemblyName PresentationCore
Add-Type -AssemblyName WindowsBase

$script:ArtifactDirectory = [IO.Path]::GetFullPath($ArtifactDirectory)
if (-not [IO.Directory]::Exists($script:ArtifactDirectory)) {
    throw 'The caller must create the isolated artifact directory first.'
}
$script:StatePath = Join-Path $script:ArtifactDirectory 'state.json'
$script:StopPath = Join-Path $script:ArtifactDirectory 'stop'
$script:Utf8 = New-Object Text.UTF8Encoding($false)
$script:ApplyCount = 0
$script:CheckboxChanges = 0
$script:ScrollOffset = 0.0
$script:FixtureClosing = $false

function Write-FixtureJson([string]$Path, [object]$Value) {
    $temporaryPath = $Path + '.tmp'
    $json = ConvertTo-Json -InputObject $Value -Depth 5 -Compress
    [IO.File]::WriteAllText($temporaryPath, $json, $script:Utf8)
    if ([IO.File]::Exists($Path)) {
        [IO.File]::Replace($temporaryPath, $Path, [NullString]::Value)
    } else {
        [IO.File]::Move($temporaryPath, $Path)
    }
}

[xml]$fixtureXaml = @'
<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Title="Nuphus isolated UIA fixture" Width="680" Height="620"
        Left="40" Top="40" WindowStartupLocation="Manual"
        AutomationProperties.AutomationId="fixture-window">
  <Grid Margin="16">
    <Grid.RowDefinitions>
      <RowDefinition Height="Auto"/><RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/><RowDefinition Height="Auto"/>
      <RowDefinition Height="Auto"/><RowDefinition Height="*"/>
    </Grid.RowDefinitions>
    <TextBlock Text="Isolated desktop acceptance fixture" FontSize="18" Margin="0,0,0,12"/>
    <TextBox x:Name="FixtureText" Grid.Row="1" Height="32" Margin="0,0,0,10"
             AutomationProperties.AutomationId="fixture-text"
             AutomationProperties.Name="Fixture text" Text="initial value"/>
    <CheckBox x:Name="FixtureCheck" Grid.Row="2" IsThreeState="True" IsChecked="{x:Null}"
              Margin="0,0,0,10" Content="Fixture checkbox"
              AutomationProperties.AutomationId="fixture-check"/>
    <Slider x:Name="FixtureRange" Grid.Row="3" Minimum="0" Maximum="100" Value="10"
            Height="32" Margin="0,0,0,10" IsSnapToTickEnabled="False"
            AutomationProperties.AutomationId="fixture-range"
            AutomationProperties.Name="Fixture range"/>
    <Button x:Name="FixtureApply" Grid.Row="4" Height="32" Margin="0,0,0,10"
            Content="Fixture apply" AutomationProperties.AutomationId="fixture-apply"/>
    <ListBox x:Name="FixtureItems" Grid.Row="5"
             ScrollViewer.VerticalScrollBarVisibility="Visible"
             ScrollViewer.CanContentScroll="False"
             VirtualizingStackPanel.IsVirtualizing="False"
             AutomationProperties.AutomationId="fixture-list"
             AutomationProperties.Name="Fixture items"/>
  </Grid>
</Window>
'@
$reader = New-Object Xml.XmlNodeReader($fixtureXaml)
$script:Fixture = [Windows.Markup.XamlReader]::Load($reader)
$script:TextBox = $script:Fixture.FindName('FixtureText')
$script:CheckBox = $script:Fixture.FindName('FixtureCheck')
$script:Range = $script:Fixture.FindName('FixtureRange')
$script:Apply = $script:Fixture.FindName('FixtureApply')
$script:Items = $script:Fixture.FindName('FixtureItems')

for ($index = 0; $index -lt 48; $index++) {
    $item = New-Object Windows.Controls.ListBoxItem
    $item.Content = 'Fixture row ' + $index
    $item.Height = 28
    [Windows.Automation.AutomationProperties]::SetAutomationId($item, 'fixture-row-' + $index)
    $null = $script:Items.Items.Add($item)
}

function Save-FixtureState {
    $checked = $null
    if ($null -ne $script:CheckBox.IsChecked) { $checked = [bool]$script:CheckBox.IsChecked }
    $lastRowFullyVisible = $false
    try {
        $lastRow = $script:Items.Items[$script:Items.Items.Count - 1]
        $point = $lastRow.TransformToAncestor($script:Items).Transform([Windows.Point]::new(0, 0))
        $lastRowFullyVisible = $point.Y -ge 0 -and ($point.Y + $lastRow.ActualHeight) -le $script:Items.ActualHeight
    } catch {
        # Initial layout has not yet attached the row to its visual ancestor.
    }
    Write-FixtureJson $script:StatePath ([ordered]@{
        text = [string]$script:TextBox.Text
        checked = $checked
        checkbox_changes = $script:CheckboxChanges
        range = [double]$script:Range.Value
        scroll_offset = [double]$script:ScrollOffset
        apply_count = $script:ApplyCount
        selected_index = [int]$script:Items.SelectedIndex
        last_row_fully_visible = [bool]$lastRowFullyVisible
    })
}

$script:TextBox.Add_TextChanged({ Save-FixtureState })
$checkboxChanged = {
    $script:CheckboxChanges++
    Save-FixtureState
}
$script:CheckBox.Add_Checked($checkboxChanged)
$script:CheckBox.Add_Unchecked($checkboxChanged)
$script:CheckBox.Add_Indeterminate($checkboxChanged)
$script:Range.Add_ValueChanged({ Save-FixtureState })
$script:Apply.Add_Click({ $script:ApplyCount++; Save-FixtureState })
$script:Items.Add_SelectionChanged({ Save-FixtureState })
$script:Items.AddHandler(
    [Windows.Controls.ScrollViewer]::ScrollChangedEvent,
    [Windows.Controls.ScrollChangedEventHandler]{
        param($sender, $eventArgs)
        $script:ScrollOffset = [double]$eventArgs.VerticalOffset
        Save-FixtureState
    }
)

# This second, equally disposable window stands in for the user's active app.
# UIA operations on Fixture must leave this window foreground and the mouse still.
$script:Sentinel = New-Object Windows.Window
$script:Sentinel.Title = 'Nuphus foreground sentinel - do not interact during test'
$script:Sentinel.Width = 460
$script:Sentinel.Height = 160
$script:Sentinel.Left = 740
$script:Sentinel.Top = 80
$script:Sentinel.WindowStartupLocation = [Windows.WindowStartupLocation]::Manual
$sentinelText = New-Object Windows.Controls.TextBlock
$sentinelText.Text = 'The other test window is operated through native UIA in the background.'
$sentinelText.TextWrapping = [Windows.TextWrapping]::Wrap
$sentinelText.Margin = New-Object Windows.Thickness(16)
$script:Sentinel.Content = $sentinelText

function Close-Fixture {
    if ($script:FixtureClosing) { return }
    $script:FixtureClosing = $true
    $script:StopTimer.Stop()
    $script:Fixture.Close()
    $script:Sentinel.Close()
}

$script:Sentinel.Add_ContentRendered({
    $null = $script:Sentinel.Activate()
    Save-FixtureState
    $targetInterop = New-Object Windows.Interop.WindowInteropHelper($script:Fixture)
    $sentinelInterop = New-Object Windows.Interop.WindowInteropHelper($script:Sentinel)
    Write-FixtureJson (Join-Path $script:ArtifactDirectory 'ready.json') ([ordered]@{
        target_hwnd = $targetInterop.Handle.ToInt64()
        sentinel_hwnd = $sentinelInterop.Handle.ToInt64()
        process_id = $PID
    })
})
$script:Sentinel.Add_Closed({ Close-Fixture })
$script:Fixture.Add_Closed({ Close-Fixture })
$script:Deadline = [DateTime]::UtcNow.AddMinutes(3)
$script:StopTimer = New-Object Windows.Threading.DispatcherTimer
$script:StopTimer.Interval = [TimeSpan]::FromMilliseconds(150)
$script:StopTimer.Add_Tick({
    if ([IO.File]::Exists($script:StopPath) -or [DateTime]::UtcNow -ge $script:Deadline) {
        Close-Fixture
    }
})
$script:StopTimer.Start()
$script:Fixture.Show()
$null = $script:Sentinel.ShowDialog()
