import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui
import "Model.js" as Model

// Bar label: 🥜 balance (⏱session countdown while a TollGate session runs).
// Left click opens the wallet panel, right click forces a refresh.
BarWidget {
  id: root

  moduleName: "nutbar"

  property bool daemonUp: false
  property int balanceSats: -1
  property var session: null
  property string mintHost: ""

  readonly property int refreshIntervalSec: Math.max(5, parseInt(String(setting("refreshIntervalSec", 10)), 10) || 10)
  readonly property string displayText: daemonUp
    ? Model.barLabel(balanceSats, session)
    : "🥜 off"

  function refresh() {
    if (statusProcess.running) return
    statusProcess.running = true
  }

  function handleStatus(raw) {
    var st = Model.parseStatus(raw)
    daemonUp = st.up
    balanceSats = st.balanceSats
    session = st.session
    mintHost = Model.hostOf(st.mint)
  }

  // ---- panel plumbing (official Quattro bar-widget contract) ----
  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false
  readonly property bool popoutSwitchClosing: panelLoader.item
    ? panelLoader.item.popoutSwitchClosing === true
    : false

  function open() {
    if (panelLoader.item) panelLoader.item.open()
  }

  function close() {
    if (panelLoader.item) panelLoader.item.close()
  }

  function toggle() {
    if (panelLoader.item) panelLoader.item.toggle()
  }

  // legacy alias — pre-Quattro IPC callers used togglePanel()
  function togglePanel() {
    root.toggle()
  }

  function closeForPopoutSwitch() {
    // guarded: the bench VM's 4.0.4 Panel base predates this hook
    var target = panelLoader.item
    if (target && typeof target.closeForPopoutSwitch === "function")
      target.closeForPopoutSwitch()
  }

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  Timer {
    id: refreshTimer
    interval: root.refreshIntervalSec * 1000
    repeat: true
    running: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  // Session countdown tick (label updates even between daemon polls).
  Timer {
    id: sessionTick
    interval: 1000
    repeat: true
    running: root.session !== null && root.session.metric === "milliseconds"
    onTriggered: {
      if (!root.session) return
      var remaining = root.session.remainingMs - 1000
      if (remaining <= 0) {
        root.session = null
      } else {
        var s = root.session
        root.session = {
          sessionId: s.sessionId,
          remaining: remaining,
          remainingMs: remaining,
          used: s.used,
          allotment: s.allotment,
          costSats: s.costSats,
          gateway: s.gateway,
          metric: s.metric,
          creditObserved: s.creditObserved
        }
      }
    }
  }

  Process {
    id: statusProcess
    running: false
    command: ["curl", "-s", "--max-time", "3", "http://127.0.0.1:3939/status"]
    stdout: StdioCollector { onStreamFinished: root.handleStatus(text) }
    stderr: StdioCollector { }
    onExited: function(exitCode) {
      if (exitCode !== 0 && root.daemonUp) root.daemonUp = false
    }
  }

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  IpcHandler {
    target: "nutbar"

    function refresh(): void { root.refresh() }
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.togglePanel() }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.displayText

    onPressed: function(b) {
      if (b === Qt.RightButton) root.refresh()
      else root.toggle()
    }
  }
}
