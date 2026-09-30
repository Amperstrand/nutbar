import QtQuick
import qs.Commons
import qs.Ui

Rectangle {
  id: tab
  property string label: ""
  property bool active: false
  signal tabActivated()
  color: active ? "#4c5a8f" : "transparent"
  border.width: active ? 0 : 1
  border.color: Qt.darker(Color.foreground, 2.2)
  radius: 8
  width: tabLabel.implicitWidth + Style.space(20)
  height: Style.space(26)
  Text {
    id: tabLabel
    anchors.centerIn: parent
    text: tab.label
    color: tab.active ? Color.foreground : Qt.darker(Color.foreground, 1.55)
    font.family: Style.font.family
    font.pixelSize: Style.font.bodySmall
  }
  MouseArea { anchors.fill: parent; cursorShape: Qt.PointingHandCursor; onClicked: tab.activated() }
}
