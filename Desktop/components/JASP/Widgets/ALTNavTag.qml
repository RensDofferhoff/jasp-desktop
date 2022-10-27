import QtQuick 2.11
import QtQuick.Controls
import JASP.Widgets

//Text tag shown when alt navigation mode is enabled.

AltNavTagBase
{
	id:					tagRoot
	visible:			show && parent.visible
	width:				tagText.contentWidth + jaspTheme.contentMargin
	height:				tagText.contentHeight
	z:					99999

	Rectangle
	{
		color:				"black"
		anchors.fill:		parent

		Text
		{
			id:							tagText
			text:						tagString
			color:						"white"
			font:						jaspTheme.fontLabel
			anchors.centerIn:			parent
			horizontalAlignment:		Text.AlignHCenter
			verticalAlignment:			Text.AlignVCenter
		}

	}
}
