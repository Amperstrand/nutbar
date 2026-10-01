import QtQuick
import QtQuick.Controls
import qs.Commons
import qs.Ui

TextField {
  color: Color.foreground
  font.family: Style.font.family
  font.pixelSize: Style.font.bodySmall
  background: Rectangle { color: "#262b40"; radius: 6 }
  implicitWidth: Style.space(64)
  implicitHeight: Style.space(30)
  leftInset: 8
  rightInset: 8
}
