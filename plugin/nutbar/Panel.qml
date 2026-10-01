import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui
import "Model.js" as Model

// Cashu wallet panel — wallet.cashu.me/eNuts-inspired flows on the Omarchy
// bar contract. All money logic lives in cashud (127.0.0.1:3939); the panel
// is curl→JSON plus qrencode/zbarimg for QR display and region-scan.
//
// Sections: hero · session · wifi/tollgate (PRIMARY — above the fold) ·
// error · SEND (token | lightning) · RECEIVE (redeem | lightning) ·
// invoice) · history · stash · wifi/tollgate. Payloads are auto-detected
// (cashu… token, lnbc… invoice) wherever they are pasted or scanned.
Panel {
  id: root

  moduleName: "nutbar"
  ipcTarget: "nutbar"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root

  // ---- wallet state ----
  property int balanceSats: -1
  property string mintHost: ""
  property var session: null
  property var historyEntries: []
  property string actionStatus: ""
  property string lastError: ""
  property int stashTokens: 0
  property bool pendingPayment: false
  property int quarantinedPayments: 0
  property bool autopay: false
  property int renewals: 0
  property var tollgates: []
  property string expiredGateway: ""
  property string scanResult: ""
  property var wifiInfo: null
  readonly property bool onTollgateAp: (wifiInfo && wifiInfo.on_tollgate_ap === true) ? true : false
  readonly property string gatewayIp: (wifiInfo && wifiInfo.gateway_ip) ? wifiInfo.gateway_ip : ""

  // ---- send/receive state machines ----
  property string sendTab: "token"        // token | invoice
  property string recvTab: "invoice"      // invoice | redeem
  property string tokenOut: ""
  property string tokenQrPath: ""
  property var invoice: null               // {id, invoice, amount_sats}
  property string invoiceState: "idle"    // idle|creating|waiting|completing|done
  property string invoiceQrPath: ""
  property string payInput: ""
  property var payParsed: null
  property string payState: "idle"        // idle|paying|paid
  property var payResult: null
  property string recvInput: ""
  property string recvState: "idle"       // idle|redeeming|done
  property var recvResult: null
  property bool scanBusy: false
  property var quotePreview: null
  property var tokenPreview: null
  property var mintInfo: null
  property bool mintExpanded: false
  property var mints: []
  property string selectedMint: ""
  property bool showSeedDialog: false
    property string seedHex: ""
    property string seedMnemonic: ""
    property string fullscreenQrSource: ""
    property string fullscreenQrTitle: ""
    property string fullscreenQrSubtitle: ""
    property string fullscreenQrCopyText: ""
  property var qrFrames: []
  property int qrFrameIndex: 0
  property int qrIntervalMs: 250
  property bool qrAnimated: false
  property bool balanceHidden: false

  readonly property color fg: bar ? bar.foreground : Color.foreground
  readonly property color dim: Qt.darker(fg, 1.55)
  readonly property color accent: Color.accent
  readonly property color good: "#9ece6a"
  readonly property color bad: "#f7768e"
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property bool daemonUp: balanceSats >= 0

  function refresh() {
    if (!statusProcess.running) statusProcess.running = true
    if (!historyProcess.running) historyProcess.running = true
  }

  function open() {
    // errors belong to the session that produced them — a stale error from
    // hours ago must not greet the next open (also gives tests a clean slate)
    lastError = ""
    refresh()
    if (!mintInfoProcess.running) mintInfoProcess.running = true
    root.controller.show()
  }

  function close() {
    root.controller.hide()
  }

  function toggle() {
    if (root.opened) root.close()
    else root.open()
  }

  function flash(text) {
    actionStatus = text
    actionStatusTimer.restart()
  }

  function post(path, payload, process) {
    process.command = [
      "curl", "-s", "--max-time", "90", "--json",
      JSON.stringify(payload),
      "http://127.0.0.1:3939" + path
    ]
    process.running = true
  }

  // ---- QR generation (qrencode → PNG → Image) ----
  function makeQr(payload, outPathProperty) {
    var path = "/tmp/cashu-qr-" + Date.now() + ".png"
    qrGenProcess.qrPath = path
    qrGenProcess.outPathProperty = outPathProperty
    qrGenProcess.command = ["qrencode", "-t", "PNG", "-s", "10", "-o", path, payload]
    qrGenProcess.running = true
  }

  // ---- scan (region screenshot decode via omarchy) ----
  function scan() {
    if (scanBusy) return
    scanBusy = true
    scanResult = ""
    scanProcess.running = true
  }

  function routePayload(text) {
    var trimmed = String(text || "").trim()
    if (trimmed === "") return
    var kind = Model.detectPayload(trimmed)
    if (kind === "token") {
      recvTab = "redeem"
      recvInput = trimmed
      recvState = "idle"
      root.tokenPreview = null
      root.post("/token-info", { token: trimmed }, tokenInfoProcess)
      flash("Ecash token detected — press Redeem")
    } else if (kind === "invoice") {
      sendTab = "invoice"
      payInput = trimmed
      parseInvoice(trimmed)
    } else {
      lastError = "Unrecognized payload (expected cashu token or lightning invoice)"
    }
  }

  // ---- flows ----
  function createToken() {
    var amount = parseInt(sendAmountField.text, 10)
    if (!isFinite(amount) || amount <= 0) amount = 21
    actionStatus = "Creating ecash token…"
    lastError = ""
    tokenOut = ""
    tokenQrPath = ""
    post("/send", { amount_sats: amount, mint: root.selectedMint !== "" ? root.selectedMint : undefined }, sendTokenProcess)
  }

  function createInvoice() {
    var amount = parseInt(invAmountField.text, 10)
    if (!isFinite(amount) || amount <= 0) { lastError = "Enter a whole number of sats"; return }
    actionStatus = "Creating invoice…"
    lastError = ""
    invoice = null
    invoiceState = "creating"
    post("/invoice", { amount_sats: amount, description: invDescField.text, mint: root.selectedMint !== "" ? root.selectedMint : undefined }, invoiceProcess)
  }

  function parseInvoice(text) {
    payParsed = null
    payState = "idle"
    actionStatus = "Reading invoice…"
    post("/parse-invoice", { invoice: text }, parseInvProcess)
  }

  function payInvoice() {
    payState = "paying"
    actionStatus = "Paying invoice…"
    post("/pay-invoice", {
      invoice: payInput,
      quote_id: root.quotePreview ? root.quotePreview.id : undefined
    }, payInvProcess)
  }

  function redeem() {
    recvState = "redeeming"
    actionStatus = "Redeeming token…"
    post("/receive", { token: recvInput }, redeemProcess)
  }

  function doTollgate() {
    actionStatus = onTollgateAp && gatewayIp !== "" ? "Paying TollGate " + gatewayIp + "…" : "Paying TollGate (local mock)…"
    post("/tollgate/pay", {}, tollgateProcess)
  }

  function doAddMint() {
    var url = addMintField.text.trim()
    if (url === "") return
    actionStatus = "Adding mint…"
    post("/mints/add", { url: url }, addMintProcess)
  }

  function doShowSeed() {
    if (showSeedDialog) return
    showSeedDialog = true
    actionStatus = ""
    seedProcess.running = true
  }

  function doPrime() {
    actionStatus = "Priming offline stash…"
    post("/stash/prime", {}, primeProcess)
  }

  function copyText(text, label) {
    var value = String(text || "")
    if (value === "") return
    Quickshell.execDetached(["bash", "-c", "printf %s " + Util.shellQuote(value) + " | wl-copy"])
    flash((label || "Copied") + " to clipboard")
  }

  Timer {
    id: pollTimer
    interval: 5000
    repeat: true
    running: root.opened
    onTriggered: root.refresh()
  }

  Timer {
    id: sessionTick
    interval: 1000
    repeat: true
    running: root.session !== null && root.opened
    onTriggered: {
      if (!root.session) return
      var remaining = root.session.remainingMs !== undefined ? root.session.remainingMs : root.session.remaining
      remaining = (remaining || 0) - 1000
      if (remaining <= 0) {
        root.session = null
      } else {
        var s = root.session
        s.remainingMs = remaining
        s.remaining = remaining
        root.session = s
      }
    }
  }

  // Lightning receive: poll invoice until paid, then auto-mint.
  Timer {
    id: invoicePollTimer
    interval: 3000
    repeat: true
    running: root.opened && root.invoiceState === "waiting"
    onTriggered: {
      if (!root.invoice) return
      invoiceStatusProcess.command = [
        "curl", "-s", "--max-time", "10",
        "http://127.0.0.1:3939/invoice/status?id=" + encodeURIComponent(root.invoice.id)
      ]
      invoiceStatusProcess.running = true
    }
  }

  Timer {
    id: actionStatusTimer
    interval: 3500
    repeat: false
    onTriggered: root.actionStatus = ""
  }

  // ---- processes ----

  Process {
    id: statusProcess
    running: false
    command: ["curl", "-s", "--max-time", "3", "http://127.0.0.1:3939/status"]
    stdout: StdioCollector {
      onStreamFinished: {
        var st = Model.parseStatus(text)
        root.balanceSats = st.up ? st.balanceSats : -1
        root.mintHost = st.mint ? Model.hostOf(st.mint) : ""
        root.session = st.session
        root.stashTokens = st.stashTokens
        root.autopay = st.autopay
        root.renewals = st.renewals
        root.quarantinedPayments = st.quarantinedPayments
        // same guard as historyEntries: a fresh array per poll would
        // reset the mint-pill Repeater and the panel scroll position
        var nextMints = st.mints || []
        if (JSON.stringify(nextMints) !== JSON.stringify(root.mints))
          root.mints = nextMints
        if (root.selectedMint === "" && root.mints.length > 0) {
          for (var i = 0; i < root.mints.length; i++) {
            if (root.mints[i].is_default) { root.selectedMint = root.mints[i].url; break }
          }
        }
        root.wifiInfo = st.wifi
        root.expiredGateway = st.expiredGateway
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: historyProcess
    running: false
    command: ["curl", "-s", "--max-time", "3", "http://127.0.0.1:3939/history"]
    stdout: StdioCollector {
      onStreamFinished: {
        var parsed = Model.parseJson(text)
        var next = parsed && parsed.entries ? parsed.entries : []
        // Reassigning an identical-but-fresh array resets the history
        // Repeater, collapses contentHeight and snaps the Flickable to
        // the top every poll — only reassign on real change.
        if (JSON.stringify(next) !== JSON.stringify(root.historyEntries))
          root.historyEntries = next
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: sendTokenProcess
    running: false
    command: []
    property string pendingPayload: ""
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.tokenOut = String(r.parsed.token || "")
          root.balanceSats = r.parsed.balance_sats
          root.makeQr(root.tokenOut, "tokenQrPath")
          root.flash(Model.fmtSats(r.parsed.sent_sats) + " sats ready to send — scan or copy")
        } else {
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: invoiceProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.invoice = r.parsed
          root.invoiceState = "waiting"
          root.makeQr(r.parsed.invoice, "invoiceQrPath")
          root.flash("Invoice ready — waiting for payment…")
        } else {
          root.invoiceState = "idle"
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: invoiceStatusProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok && r.parsed.paid === true && root.invoiceState === "waiting") {
          root.invoiceState = "completing"
          root.post("/invoice/complete", { quote_id: root.invoice.id }, invoiceCompleteProcess)
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: invoiceCompleteProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.invoiceState = "done"
          root.flash("✓ " + r.parsed.minted_sats + " sats received")
          root.refresh()
        } else {
          root.invoiceState = "waiting"
          root.lastError = r.error
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: parseInvProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.payParsed = r.parsed
          root.quotePreview = null
          root.post("/melt-quote", { invoice: root.payInput }, meltQuoteProcess)
          root.actionStatus = ""
        } else {
          root.payParsed = null
          root.quotePreview = null
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: meltQuoteProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        root.quotePreview = r.ok ? r.parsed : null
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: mintInfoProcess
    running: false
    command: ["curl", "-s", "--max-time", "10", "http://127.0.0.1:3939/mint-info"]
    stdout: StdioCollector {
      onStreamFinished: {
        var parsed = Model.parseJson(text)
        root.mintInfo = parsed && parsed.ok ? parsed : null
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: payInvProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.payState = "paid"
          root.payResult = r.parsed
          root.flash("✓ Paid " + r.parsed.paid_sats + " sats (fee " + r.parsed.fee_sats + ")")
          root.refresh()
        } else {
          root.payState = "idle"
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: tokenInfoProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        root.tokenPreview = r.ok ? r.parsed : null
        if (!r.ok) {
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: redeemProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.recvState = "done"
          root.recvResult = r.parsed
          root.tokenPreview = null
          root.flash("✓ Received " + r.parsed.received_sats + " sats")
          root.refresh()
        } else {
          root.recvState = "idle"
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: qrGenProcess
    running: false
    property string qrPath: ""
    property string outPathProperty: ""
    command: []
    stdout: StdioCollector { }
    stderr: StdioCollector { }
    onExited: function(exitCode) {
      if (exitCode === 0) {
        if (outPathProperty === "tokenQrPath") root.tokenQrPath = qrPath
        if (outPathProperty === "invoiceQrPath") root.invoiceQrPath = qrPath
      } else {
        root.lastError = "QR generation failed"
      }
    }
  }

  Process {
    id: scanProcess
    running: false
    command: ["omarchy", "capture", "qr"]
    stdout: StdioCollector {
      onStreamFinished: {
        root.scanBusy = false
        var decoded = String(text || "").trim()
        if (decoded !== "") root.routePayload(decoded)
      }
    }
    stderr: StdioCollector {
      onStreamFinished: { root.scanBusy = false }
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) root.scanBusy = false
    }
  }

  Process {
    id: tollgateProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.session = Model.parseSession(r.parsed.session)
          root.refresh()
          root.flash("TollGate session active ⏱")
        } else {
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: qrFramesProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var parsed = Model.parseJson(text)
        if (parsed && parsed.ok) {
          root.qrAnimated = parsed.animated === true
          root.qrFrames = parsed.frames || []
          root.qrFrameIndex = 0
          root.qrIntervalMs = parsed.interval_ms || 250
          if (root.qrAnimated && root.qrFrames.length > 1) {
            qrAnimation.restart()
          }
        } else {
          root.qrAnimated = false
          root.qrFrames = []
        }
      }
    }
    stderr: StdioCollector { }
  }

  Timer {
    id: qrAnimation
    interval: Math.max(100, root.qrIntervalMs)
    repeat: true
    running: false
    onTriggered: {
      if (root.qrFrames.length === 0) return
      root.qrFrameIndex = (root.qrFrameIndex + 1) % root.qrFrames.length
    }
  }

  Process {
    id: addMintProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.addMintField.text = ""
          root.flash("Mint added")
          root.refresh()
        } else {
          root.lastError = r.error
          root.actionStatus = ""
        }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: seedProcess
    running: false
    command: ["curl", "-s", "--max-time", "5", "--json", '{"confirm":"show seed"}', "http://127.0.0.1:3939/seed"]
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) {
          root.seedHex = r.parsed.seed_hex || ""
          root.seedMnemonic = r.parsed.mnemonic || ""
        }
        else { root.lastError = r.error; root.showSeedDialog = false }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: primeProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) root.flash("Stash primed → " + r.parsed.count + " tokens")
        else { root.lastError = r.error; root.actionStatus = "" }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: wifiScanProcess
    running: false
    command: ["curl", "-s", "--max-time", "25", "http://127.0.0.1:3939/wifi"]
    stdout: StdioCollector {
      onStreamFinished: {
        var parsed = Model.parseJson(text)
        if (!parsed || !parsed.ok) { root.scanResult = "scan failed"; root.tollgates = []; return }
        if (!parsed.enabled) { root.scanResult = "wifi disabled (CASHUD_WIFI=1)"; root.tollgates = []; return }
        root.tollgates = parsed.tollgates || []
        root.scanResult = root.tollgates.length === 0 ? "no TollGate-* SSIDs in range" : ""
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: wifiConnectProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) root.flash("Connected — gateway will auto-detect on pay")
        else { root.lastError = r.error; root.actionStatus = "" }
      }
    }
    stderr: StdioCollector { }
  }

  Process {
    id: fallbackProcess
    running: false
    command: []
    stdout: StdioCollector {
      onStreamFinished: {
        var r = Model.parseAction(text)
        if (r.ok) root.flash("Fallback → " + (r.parsed.fallback || "active connection"))
        else { root.lastError = r.error; root.actionStatus = "" }
      }
    }
    stderr: StdioCollector { }
  }

  // ---- layout ----

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(400))
    contentHeight: panel.fittedContentHeight(panelColumn.implicitHeight, Style.space(620))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      blocked: sendAmountField.activeFocus || invAmountField.activeFocus || invDescField.activeFocus
        || payField.activeFocus || recvField.activeFocus
      onCloseRequested: {
        if (root.fullscreenQrSource !== "") {
          root.fullscreenQrSource = ""
          root.fullscreenQrCopyText = ""
        } else {
          root.close()
        }
      }
      onTabRequested: function(direction) {
        if (root.bar && typeof root.bar.switchPanelFrom === "function")
          root.bar.switchPanelFrom(root.barIdentity, direction)
      }

      CashuQrOverlay {
        id: fullscreenQr
        source: root.qrAnimated && root.qrFrames.length > 0
          ? root.qrFrames[root.qrFrameIndex % root.qrFrames.length].path || root.fullscreenQrSource
          : root.fullscreenQrSource
        title: root.fullscreenQrTitle
        subtitle: root.qrAnimated
          ? root.fullscreenQrSubtitle + " (animated QR — " + root.qrFrames.length + " frames)"
          : root.fullscreenQrSubtitle
        showWaiting: root.invoiceState === "waiting" && root.fullscreenQrSource === root.invoiceQrPath
        z: 100
        onCopyRequested: {
          root.copyText(root.fullscreenQrCopyText, "Payload")
        }
        onCloseRequested: {
          root.fullscreenQrSource = ""
          root.fullscreenQrCopyText = ""
          root.qrAnimated = false
          root.qrFrames = []
          qrAnimation.stop()
        }
      }

      Flickable {
        id: panelFlick
        anchors.fill: parent
        contentWidth: width
        contentHeight: panelColumn.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        flickableDirection: Flickable.VerticalFlick
        interactive: contentHeight > height
        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

        Column {
          id: panelColumn
          width: panelFlick.width
          spacing: Style.space(12)

          // ============ hero ============
          Column {
            spacing: Style.space(2)
            width: parent.width

            CashuSectionLabel { text: "CASHU WALLET" }

            Text {
              Accessible.role: Accessible.StaticText
              Accessible.name: !root.daemonUp
                ? "Wallet app not running"
                : (root.balanceHidden ? "balance hidden" : Model.fmtSats(root.balanceSats) + " sat")
              text: !root.daemonUp
                ? "Wallet app not running"
                : (root.balanceHidden ? "•••••" : Model.fmtSats(root.balanceSats) + " sat")
              color: root.fg
              font.family: root.fontFamily
              font.pointSize: Style.font.display

              MouseArea {
                anchors.fill: parent
                cursorShape: Qt.PointingHandCursor
                onClicked: root.balanceHidden = !root.balanceHidden
              }
            }

            Text {
              text: root.daemonUp
                ? (root.mintHost !== "" ? "Money issued by: " + root.mintHost : "")
                : "Your balance is saved on this computer. Start the wallet:"
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
            }

            Text {
              visible: !root.daemonUp
              text: "systemctl --user start omarchy-cashud"
              color: root.dim
              font.family: "monospace"
              font.pixelSize: Style.font.bodySmall
            }

            // Multi-mint selector
            Row {
              visible: root.mints.length > 1 || root.mints.length > 0
              spacing: Style.space(4)
              width: parent.width

              Repeater {
                model: root.mints

                Rectangle {
                  required property var modelData
                  width: mintPillLabel.implicitWidth + Style.space(16)
                  height: Style.space(24)
                  radius: 12
                  color: modelData.url === root.selectedMint ? "#4c5a8f" : "transparent"
                  border.width: modelData.url === root.selectedMint ? 0 : 1
                  border.color: Qt.darker(root.fg, 2.2)

                  Text {
                    id: mintPillLabel
                    anchors.centerIn: parent
                    text: Model.hostOf(modelData.url) + " · " + Model.fmtSats(modelData.balance_sats)
                    color: modelData.url === root.selectedMint ? root.fg : root.dim
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                  }

                  MouseArea {
                    anchors.fill: parent
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.selectedMint = modelData.url
                  }
                }
              }
            }

            // Add mint + view seed
            Row {
              spacing: Style.space(8)
              width: parent.width

              CashuWideField {
                id: addMintField
                placeholderText: "Add mint URL (https://…)"
                implicitWidth: panelColumn.width - addMintBtn.width - seedBtn.width - Style.space(24)
              }

              CashuPillButton {
                id: addMintBtn
                label: "Add"
                anchors.verticalCenter: parent.verticalCenter
                onPillActivated: root.doAddMint()
              }

              CashuPillButton {
                id: seedBtn
                label: "🔑"
                anchors.verticalCenter: parent.verticalCenter
                onPillActivated: root.doShowSeed()
              }
            }

            // Custody hint — adding a mint hands your money to a new
            // custodian; say so where the action happens.
            Text {
              text: "A mint holds your money and can refuse to pay out. Only add mints you trust."
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WrapAnywhere
              width: parent.width
            }

            // Seed dialog
            Rectangle {
              visible: root.showSeedDialog
              width: parent.width
              height: visible ? seedCol.implicitHeight + Style.space(20) : 0
              radius: 10
              color: "#2a1f2b"

              Column {
                id: seedCol
                anchors.fill: parent
                anchors.margins: Style.space(10)
                spacing: Style.space(6)

                Text {
                  text: "⚠ Seed phrase — anyone who can see it controls your wallet"
                  color: root.bad
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  font.letterSpacing: 0.5
                  wrapMode: Text.WrapAnywhere
                  width: parent.width
                }

                Text {
                  text: root.seedMnemonic !== ""
                    ? root.seedMnemonic
                    : (root.seedHex !== "" ? root.seedHex : "loading…")
                  color: root.fg
                  font.family: "monospace"
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.WrapAnywhere
                  width: parent.width
                }

                Text {
                  text: "This phrase controls your wallet keys. Today it cannot restore your balance on a new device by itself — automatic restore is not built yet. To move money to another device for now, export your balance: SEND → Send all creates a code that works like cash."
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.WrapAnywhere
                  width: parent.width
                }

                Text {
                  visible: root.seedMnemonic !== ""
                  text: "12 words (new wallets) — write them down in order. Legacy wallets: back up the hex seed instead."
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.WrapAnywhere
                  width: parent.width
                }

                Row {
                  spacing: Style.space(8)
                  CashuPillButton {
                    label: root.seedMnemonic !== "" ? "Copy words" : "Copy seed"
                    onPillActivated: root.copyText(root.seedMnemonic !== "" ? root.seedMnemonic : root.seedHex, "Seed")
                  }
                  CashuPillButton { label: "Close"; onPillActivated: { root.showSeedDialog = false; root.seedHex = ""; root.seedMnemonic = "" } }
                }

                Text {
                  text: "Store offline (paper/password manager). Never share."
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.WrapAnywhere
                  width: parent.width
                }
              }
            }

            Column {
              visible: root.mintInfo !== null
              spacing: 1
              width: parent.width

              Text {
                text: root.mintInfo
                  ? root.mintInfo.name + " · " + root.mintInfo.keysets.active + "/" + root.mintInfo.keysets.total + " keysets " + (root.mintExpanded ? "▴" : "▾")
                  : ""
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: root.mintExpanded = !root.mintExpanded }
              }

              Column {
                visible: root.mintExpanded
                spacing: 1
                width: parent.width

                Text {
                  text: root.mintInfo ? ("v" + root.mintInfo.version + " · pubkey " + root.mintInfo.pubkey) : ""
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  elide: Text.ElideMiddle
                  width: parent.width
                }
              }
            }
          }

          // ============ error card ============
          Rectangle {
            visible: root.lastError !== ""
            width: parent.width
            height: visible ? errorCol.implicitHeight + Style.space(16) : 0
            radius: 10
            color: "#2a1f2b"

            Column {
              id: errorCol
              anchors.fill: parent
              anchors.margins: Style.space(10)
              spacing: 2

              Text {
                Accessible.role: Accessible.StaticText
                Accessible.name: {
                  var f = Model.friendlyError(root.lastError)
                  return f.title !== "" ? f.title : "⚠ " + root.lastError
                }
                text: {
                  var f = Model.friendlyError(root.lastError)
                  return f.title !== "" ? f.title : "⚠ " + root.lastError
                }
                color: root.bad
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                wrapMode: Text.WrapAnywhere
                width: parent.width
              }

              Text {
                visible: {
                  var f = Model.friendlyError(root.lastError)
                  return f.title !== "" && f.detail !== ""
                }
                Accessible.role: Accessible.StaticText
                Accessible.name: {
                  var f = Model.friendlyError(root.lastError)
                  return f.detail
                }
                text: {
                  var f = Model.friendlyError(root.lastError)
                  return f.detail
                }
                color: Qt.darker(root.dim, 1.3)
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                elide: Text.ElideMiddle
                width: parent.width
              }
            }
          }

          // ============ session banner ============
          Rectangle {
            visible: root.session !== null
            width: parent.width
            height: visible ? sessionCol.implicitHeight + Style.space(16) : 0
            radius: 10
            color: "#1f2335"

            Column {
              id: sessionCol
              anchors.fill: parent
              anchors.margins: Style.space(10)
              spacing: Style.space(4)

              Text {
                text: root.session ? "⏱ TollGate session · " + Model.sessionRemaining(root.session) + " left" : ""
                color: root.fg
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }

              Text {
                text: root.session ? root.session.costSats + " sat · " + root.session.gateway : ""
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
            }
          }

          // ============ wifi / tollgate ============
          Column {
            spacing: Style.space(4)
            width: parent.width

            Row {
              spacing: Style.space(8)

              CashuPillButton { label: "⌁ Scan TollGates"; onPillActivated: { root.scanResult = "scanning…"; wifiScanProcess.running = true } }
              CashuPillButton { label: "Save current Wi-Fi as fallback"; onPillActivated: root.post("/wifi/fallback", { use_active: true }, fallbackProcess) }
            }

            Rectangle {
              visible: root.onTollgateAp
              width: parent.width
              height: visible ? tgBannerCol.implicitHeight + Style.space(16) : 0
              radius: 10
              color: "#1f2b28"

              Column {
                id: tgBannerCol
                anchors.fill: parent
                anchors.margins: Style.space(8)
                spacing: Style.space(6)

                Text {
                  text: "📡 on " + (root.wifiInfo ? root.wifiInfo.active_ssid : "") + " · gateway " + (root.gatewayIp !== "" ? root.gatewayIp : "…")
                  color: root.good
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                }

                Text {
                  visible: root.expiredGateway !== "" && !root.session
                  text: "⚠ previous TollGate session ran out — pay to renew"
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.WrapAnywhere
                  width: parent.width
                }

                CashuPillButton { label: "⏱ Pay this TollGate (1 sat/min)"; onPillActivated: root.doTollgate() }

                // MAC-privacy: TollGate payments are keyed to the WiFi MAC
                Text {
                  visible: root.onTollgateAp
                  text: "Payments are linked to this device Wi-Fi address."
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  wrapMode: Text.WrapAnywhere
                  width: parent.width
                }
              }
            }

            Repeater {
              model: root.tollgates

              Row {
                required property var modelData
                width: panelColumn.width
                spacing: Style.space(8)

                Text {
                  text: modelData.ssid + "  " + modelData.signal + " dBm"
                  color: root.fg
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  elide: Text.ElideRight
                  width: parent.width - connectPill.width - Style.space(8)
                  anchors.verticalCenter: parent.verticalCenter
                }

                CashuPillButton {
                  id: connectPill
                  label: "Connect"
                  anchors.verticalCenter: parent.verticalCenter
                  onPillActivated: {
                    root.post("/wifi/connect", { ssid: modelData.ssid }, wifiConnectProcess)
                  }
                }
              }
            }

            Text {
              visible: root.scanResult !== ""
              text: root.scanResult
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WrapAnywhere
              width: parent.width
            }

            CashuPillButton { label: "⏱ Pay local mock · 127.0.0.1"; onPillActivated: root.doTollgate() }
          }

          Rectangle {
            visible: root.quarantinedPayments > 0
            width: panelColumn.width
            height: visible ? quarantinedCol.implicitHeight + Style.space(16) : 0
            radius: 10
            color: "#2a1f2b"

            Column {
              id: quarantinedCol
              anchors.fill: parent
              anchors.margins: Style.space(10)
              spacing: 2

              Text {
                text: "⚠ " + root.quarantinedPayments + " token(s) quarantined — outcome was ambiguous"
                color: root.bad
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                wrapMode: Text.WrapAnywhere
                width: parent.width
              }

              Text {
                text: "Do not delete these files. See ~/.local/share/omarchy-cashu/payment.quarantine.* for recovery"
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                wrapMode: Text.WrapAnywhere
                width: parent.width
              }
            }
          }

          // ============ SEND ============
          Column {
            spacing: Style.space(8)
            width: parent.width

            Row {
              spacing: Style.space(8)
              CashuSectionLabel { text: "SEND" }
              Item { width: Style.space(8); height: 1 }
              CashuTabButton { label: "Ecash token"; active: root.sendTab === "token"; onTabActivated: root.sendTab = "token" }
              CashuTabButton { label: "Lightning"; active: root.sendTab === "invoice"; onTabActivated: { root.sendTab = "invoice" } }
            }

            // --- send: ecash token ---
            Column {
              visible: root.sendTab === "token"
              spacing: Style.space(6)
              width: parent.width

              Row {
                spacing: Style.space(8)
                CashuAmountField {
                  id: sendAmountField
                  text: "21"
                  anchors.verticalCenter: parent.verticalCenter
                  onAccepted: root.createToken()
                }
                CashuPillButton {
                  label: "Send all"
                  anchors.verticalCenter: parent.verticalCenter
                  onPillActivated: { sendAmountField.text = root.balanceSats > 0 ? String(root.balanceSats) : "0" }
                }
                CashuPillButton { label: "Create token"; anchors.verticalCenter: parent.verticalCenter; onPillActivated: root.createToken() }
              }

              Row {
                visible: root.tokenQrPath !== "" || root.tokenOut !== ""
                spacing: Style.space(12)
                width: parent.width

                CashuQrDisplay {
                  id: tokenQr
                  source: root.tokenQrPath
                  onQrClicked: {
                    root.fullscreenQrSource = root.tokenQrPath
                    root.fullscreenQrTitle = "Send ecash"
                    root.fullscreenQrSubtitle = "Scan with any Cashu wallet — anyone who gets this code can spend it"
                    root.fullscreenQrCopyText = root.tokenOut
                    // Fetch animated QR frames for large tokens
                    if (root.tokenOut.length > 600) {
                      qrFramesProcess.command = [
                        "curl", "-s", "--max-time", "10", "--json",
                        JSON.stringify({ token: root.tokenOut }),
                        "http://127.0.0.1:3939/qr-frames"
                      ]
                      qrFramesProcess.running = true
                    }
                  }
                }

                Column {
                  spacing: Style.space(4)
                  width: parent.width - tokenQr.width - Style.space(12)

                  Text {
                    text: root.tokenOut !== "" ? Model.tokenPreview(root.tokenOut) : ""
                    color: root.dim
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                    elide: Text.ElideMiddle
                    width: parent.width
                  }

                  Text {
                    visible: root.tokenOut !== ""
                    Accessible.role: Accessible.StaticText
                    Accessible.name: "⚠ Anyone who has this code can spend it. Share it only with the person you are paying."
                    text: "⚠ Anyone who has this code can spend it. Share it only with the person you are paying."
                    color: root.bad
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                    wrapMode: Text.WrapAnywhere
                    width: parent.width
                  }

                  Row {
                    spacing: Style.space(8)
                    CashuPillButton { label: "Copy token"; onPillActivated: root.copyText(root.tokenOut, "Token — anyone with it can spend the money") }
                    CashuPillButton { label: "⌁ Scan"; labelColor: root.accent; onPillActivated: root.scan() }
                  }
                }
              }
            }

            // --- send: lightning (pay invoice) ---
            Column {
              visible: root.sendTab === "invoice"
              spacing: Style.space(6)
              width: parent.width

              Row {
                spacing: Style.space(8)
                width: parent.width

                CashuWideField {
                  id: payField
                  placeholderText: "Paste invoice (lnbc…)"
                  text: root.payInput
                  onTextChanged: root.payInput = text
                  onAccepted: {
                    if (text.trim() !== "") {
                      if (root.payParsed !== null && root.payState === "idle") root.payInvoice()
                      else root.parseInvoice(text.trim())
                    }
                  }
                  implicitWidth: panelColumn.width - payScanBtn.width - Style.space(8)
                }

                CashuPillButton {
                  id: payScanBtn
                  label: root.scanBusy ? "…" : "⌁ Scan"
                  anchors.verticalCenter: parent.verticalCenter
                  onPillActivated: root.scan()
                }
              }

              Rectangle {
                visible: root.payParsed !== null || root.payState === "paid"
                width: parent.width
                height: visible ? payPreviewCol.implicitHeight + Style.space(16) : 0
                radius: 10
                color: "#1f2335"

                Column {
                  id: payPreviewCol
                  anchors.fill: parent
                  anchors.margins: Style.space(8)
                  spacing: Style.space(4)

                  Text {
                    text: root.payState === "paid"
                      ? "✓ Paid " + (root.payResult ? root.payResult.paid_sats : "?") + " sat · fee "
                        + (root.payResult ? root.payResult.fee_sats : "?") + " sat"
                      : (root.payParsed ? "Invoice · " + root.payParsed.amount_sats + " sat"
                        + (root.payParsed.description ? " · " + root.payParsed.description : "") : "")
                    color: root.payState === "paid" ? root.good : root.fg
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                    wrapMode: Text.WrapAnywhere
                    width: parent.width
                  }

                  Text {
                    visible: root.payState === "idle" && root.quotePreview !== null
                    text: root.quotePreview
                      ? "fee up to " + root.quotePreview.fee_reserve_sats + " sat · "
                        + (root.balanceSats - root.quotePreview.amount_sats - root.quotePreview.fee_reserve_sats)
                        + " sat after"
                      : ""
                    color: root.dim
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                  }

                  Text {
                    visible: root.payState === "paid" && root.payResult && root.payResult.preimage
                    text: root.payResult && root.payResult.preimage ? "preimage " + root.payResult.preimage.substring(0, 24) + "…" : ""
                    color: root.dim
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                    elide: Text.ElideMiddle
                    width: parent.width
                  }

                  CashuPillButton {
                    visible: root.payState === "idle" && root.payParsed !== null
                    label: root.payParsed ? ("Pay " + root.payParsed.amount_sats + " sat (↵)") : "Pay"
                    labelColor: root.good
                    onPillActivated: root.payInvoice()
                  }
                }
              }
            }
          }

          // ============ RECEIVE ============
          Column {
            spacing: Style.space(8)
            width: parent.width

            Row {
              spacing: Style.space(8)
              CashuSectionLabel { text: "RECEIVE" }
              Item { width: Style.space(8); height: 1 }
              CashuTabButton { label: "Lightning"; active: root.recvTab === "invoice"; onTabActivated: root.recvTab = "invoice" }
              CashuTabButton { label: "Redeem ecash"; active: root.recvTab === "redeem"; onTabActivated: root.recvTab = "redeem" }
            }

            // --- receive: lightning invoice ---
            Column {
              visible: root.recvTab === "invoice"
              spacing: Style.space(6)
              width: parent.width

              Row {
                spacing: Style.space(8)
                CashuAmountField { id: invAmountField; text: "21"; anchors.verticalCenter: parent.verticalCenter }
                CashuWideField {
                  id: invDescField
                  placeholderText: "description (optional)"
                  implicitWidth: panelColumn.width - invAmountField.width - createBtn.width - Style.space(16)
                }
                CashuPillButton { id: createBtn; label: "Create invoice"; anchors.verticalCenter: parent.verticalCenter; onPillActivated: root.createInvoice() }
              }

              Row {
                visible: root.invoiceQrPath !== "" && root.invoiceState !== "done"
                spacing: Style.space(12)
                width: parent.width

                CashuQrDisplay {
                  id: invQr
                  source: root.invoiceQrPath
                  onQrClicked: {
                    root.fullscreenQrSource = root.invoiceQrPath
                    root.fullscreenQrTitle = "Lightning invoice"
                    root.fullscreenQrSubtitle = root.invoice ? root.invoice.amount_sats + " sat" : ""
                    root.fullscreenQrCopyText = root.invoice ? root.invoice.invoice : ""
                  }
                }

                Column {
                  spacing: Style.space(4)
                  width: parent.width - invQr.width - Style.space(12)

                  Text {
                    Accessible.role: Accessible.StaticText
                    Accessible.name: {
                      if (root.invoiceState === "creating") return "Creating invoice…"
                      if (root.invoiceState === "waiting") return "Waiting for payment…"
                      if (root.invoiceState === "completing") return "Adding to your wallet…"
                      return ""
                    }
                    text: {
                      if (root.invoiceState === "creating") return "Creating invoice…"
                      if (root.invoiceState === "waiting") return "Waiting for payment…"
                      if (root.invoiceState === "completing") return "Adding to your wallet…"
                      return ""
                    }
                    color: root.accent
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                    wrapMode: Text.WrapAnywhere
                    width: parent.width
                  }

                  Row {
                    spacing: Style.space(8)
                    CashuPillButton { label: "Copy invoice"; onPillActivated: root.copyText(root.invoice ? root.invoice.invoice : "", "Invoice") }
                  }
                }
              }

              Rectangle {
                visible: root.invoiceState === "done"
                width: parent.width
                height: doneCol.implicitHeight + Style.space(16)
                radius: 10
                color: "#1f2b28"
                Column {
                  id: doneCol
                  anchors.fill: parent
                  anchors.margins: Style.space(8)
                  Text {
                    text: "✓ Payment received — added to your wallet"
                    color: root.good
                    font.family: root.fontFamily
                    font.pixelSize: Style.font.bodySmall
                  }
                }
              }
            }

            // --- receive: redeem token ---
            Column {
              visible: root.recvTab === "redeem"
              spacing: Style.space(6)
              width: parent.width

              Row {
                spacing: Style.space(8)
                width: parent.width

                CashuWideField {
                  id: recvField
                  placeholderText: "Paste ecash token (cashu…)"
                  text: root.recvInput
                  onTextChanged: root.recvInput = text
                  onAccepted: {
                    if (text.trim() !== "") {
                      if (root.tokenPreview !== null && root.recvState === "idle") root.redeem()
                      else root.post("/token-info", { token: text.trim() }, tokenInfoProcess)
                    }
                  }
                  implicitWidth: panelColumn.width - recvScanBtn.width - recvBtn.width - Style.space(16)
                }

                CashuPillButton {
                  id: recvScanBtn
                  label: "⌁"
                  anchors.verticalCenter: parent.verticalCenter
                  onPillActivated: root.scan()
                }

                CashuPillButton {
                  id: recvBtn
                  label: root.recvState === "redeeming" ? "…" : (root.tokenPreview !== null ? "Redeem (↵)" : "Redeem")
                  anchors.verticalCenter: parent.verticalCenter
                  onPillActivated: root.redeem()
                }
              }

              Text {
                visible: root.tokenPreview !== null && root.recvState !== "done"
                text: root.tokenPreview
                  ? "✓ Detected ecash token · " + Model.fmtSats(root.tokenPreview.amount_sats) + " sat · " + Model.hostOf(root.tokenPreview.mint)
                  : ""
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                elide: Text.ElideMiddle
                width: parent.width
              }

              Text {
                visible: root.recvState === "done" && root.recvResult
                Accessible.role: Accessible.StaticText
                Accessible.name: root.recvResult ? "✓ Received " + root.recvResult.received_sats + " sats" : ""
                text: root.recvResult ? "✓ Received " + root.recvResult.received_sats + " sats" : ""
                color: root.good
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
            }
          }

          // ============ history ============
          Column {
            visible: root.historyEntries.length > 0
            spacing: Style.space(4)
            width: parent.width

            CashuSectionLabel { text: "HISTORY" }

            Repeater {
              model: root.historyEntries

              Row {
                required property var modelData
                width: panelColumn.width
                height: Style.space(24)
                spacing: Style.space(8)

                Text {
                  anchors.verticalCenter: parent.verticalCenter
                  text: Model.historyArrow(modelData.direction)
                  color: Model.historyIncoming(modelData.direction) ? root.good : root.bad
                    font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                }

                Text {
                  text: Model.fmtSats(modelData.amount_sats) + " sat"
                  color: Model.historyIncoming(modelData.direction) ? root.good : root.fg
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                }

                Text {
                  text: (modelData.memo && String(modelData.memo) !== "")
                    ? String(modelData.memo)
                    : Model.historyType(modelData.direction)
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  elide: Text.ElideRight
                  width: parent.width - Style.space(150)
                }

                Text {
                  text: modelData.timestamp ? Model.timeOf(modelData.timestamp) : ""
                  color: root.dim
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.bodySmall
                  anchors.verticalCenter: parent.verticalCenter
                }
              }
            }
          }

          Text {
            visible: root.historyEntries.length === 0 && root.daemonUp
            text: "No transactions yet — create an invoice or paste a token to get started"
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WrapAnywhere
            width: panelColumn.width
          }

          // ============ stash ============
          Column {
            spacing: Style.space(4)
            width: parent.width

            Row {
              spacing: 0
              Text {
                text: "OFFLINE STASH · "
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                font.letterSpacing: 1.2
              }
              Text {
                text: root.stashTokens + " × 1-sat"
                color: root.fg
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
              Text {
                text: root.autopay ? " · autopay (" + root.renewals + ")" : ""
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                font.letterSpacing: 1.2
              }
            }

            Row {
              spacing: Style.space(8)
              width: parent.width

              CashuPillButton { id: primeBtn; label: "Prime"; onPillActivated: root.doPrime() }

              Text {
                text: "pre-made tokens spendable with no internet"
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
                elide: Text.ElideRight
                width: parent.width - primeBtn.width - Style.space(16)
                anchors.verticalCenter: parent.verticalCenter
              }
            }
          }

          // ============ footer ============
          Text {
            visible: root.actionStatus !== ""
            text: root.actionStatus
            color: root.accent
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WrapAnywhere
            width: parent.width
          }

          Text {
            text: "⚠ Ecash works like cash: whoever holds the code holds the money. Back up your 12-word phrase (🔑). To move money to a new device today, also export your balance (SEND → Send all) — that code spends like cash."
            color: Qt.darker(root.dim, 1.2)
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WrapAnywhere
            width: parent.width
          }

          Item { width: 1; height: Style.space(10) }
        }
      }
    }
  }
}
