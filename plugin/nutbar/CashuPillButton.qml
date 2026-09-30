import QtQuick
import qs.Commons
import qs.Ui

Rectangle {
  id: pill
  property string label: ""
  property color labelColor: Color.foreground
  property color pillColor: "#2a2f44"
  signal pillActivated()
  color: mouse.containsMouse ? Qt.darker(Color.accent, 1.6) : pillColor
  radius: height / 2
  width: pillLabel.implicitWidth + Style.space(24)
  height: Style.space(30)

  Text {
    id: pillLabel
    anchors.centerIn: parent
    text: pill.label
    color: pill.labelColor
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
  }

  MouseArea {
    id: mouse
    anchors.fill: parent
    hoverEnabled: true
    cursorShape: Qt.PointingHandCursor
    onClicked: pill.activated()
  }
}
