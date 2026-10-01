import QtQuick
import qs.Commons
import qs.Ui

Rectangle {
  id: qrBox
  property string source: ""
  property string label: ""
  property string sublabel: ""
  signal qrClicked()
  visible: source !== ""
  width: Style.space(24)
  height: Style.space(24)
  radius: 8
  color: "#eef0f6"

  Image {
    anchors.centerIn: parent
    width: parent.width - Style.space(4)
    height: parent.height - Style.space(4)
    source: qrBox.source !== "" ? ("file://" + qrBox.source) : ""
    fillMode: Image.PreserveAspectFit
    smooth: false
  }

  MouseArea {
    anchors.fill: parent
    cursorShape: Qt.PointingHandCursor
    onClicked: qrBox.qrClicked()
  }
}
