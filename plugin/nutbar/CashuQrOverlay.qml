import QtQuick
import qs.Commons
import qs.Ui

Rectangle {
  id: overlay
  property string source: ""
  property string title: ""
  property string subtitle: ""
  property bool showCopy: true
  property bool showWaiting: false
  signal copyRequested()
  signal closeRequested()
  visible: source !== ""
  anchors.fill: parent
  color: "#ee0d0f1a"

  MouseArea {
    anchors.fill: parent
    onClicked: overlay.closeRequested()
  }

  Column {
    anchors.centerIn: parent
    spacing: Style.space(16)

    Text {
      anchors.horizontalCenter: parent.horizontalCenter
      text: overlay.title
      color: Color.foreground
      font.family: Style.font.family
      font.pointSize: Style.font.display
    }

    Text {
      visible: overlay.subtitle !== ""
      anchors.horizontalCenter: parent.horizontalCenter
      text: overlay.subtitle
      color: Qt.darker(Color.foreground, 1.55)
      font.family: Style.font.family
      font.pixelSize: Style.font.body
    }

    Text {
      visible: overlay.showWaiting
      anchors.horizontalCenter: parent.horizontalCenter
      text: "Awaiting payment…"
      color: Color.accent
      font.family: Style.font.family
      font.pixelSize: Style.font.bodySmall
    }

    Rectangle {
      id: qrContainer
      anchors.horizontalCenter: parent.horizontalCenter
      width: Math.min(parent.width, parent.height) * 0.85
      height: width
      radius: 16
      color: "#eef0f6"

      Image {
        anchors.centerIn: parent
        width: parent.width - Style.space(8)
        height: parent.height - Style.space(8)
        source: overlay.source !== "" ? ("file://" + overlay.source) : ""
        fillMode: Image.PreserveAspectFit
        smooth: false
      }

      MouseArea {
        anchors.fill: parent
        cursorShape: Qt.PointingHandCursor
        onClicked: overlay.copyRequested()
      }
    }

    Row {
      visible: overlay.showCopy
      anchors.horizontalCenter: parent.horizontalCenter
      spacing: Style.space(12)

      CashuPillButton {
        label: "Copy"
        onPillActivated: overlay.copyRequested()
      }

      CashuPillButton {
        label: "Close (Esc)"
        onPillActivated: overlay.closeRequested()
      }
    }
  }
}
