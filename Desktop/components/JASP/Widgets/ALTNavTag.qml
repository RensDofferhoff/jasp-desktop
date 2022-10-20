import QtQuick
import QtQuick.Controls

//Text tag shown when alt navigation mode is enabled.

Rectangle
{
	id:					tagRoot
	activeFocusOnTab:	false
	visible:			false
	width:				tagText.contentWidth + jaspTheme.contentMargin
	height:				tagText.contentHeight
	z:					99999
	color:				"black"

	required	property	var			matchActionFunc
				property	var			parentTagObject		:	null
				property	string		requestPostfix		:	""
				property	string		prefix				:	""

	Component.onCompleted:
	{
		if (prefix !== "")
			altNavigationModel.addTag(this, prefix, requestPostfix)
		else
			altNavigationModel.addTag(this, parentTagObject, requestPostfix);
	}
	Component.onDestruction:	{ altNavigationModel.removeTag(this); }

	Connections
	{
		target:	altNavigationModel
		function onAltNavInputChanged()
		{
			if (altNavigationModel.altNavEnabled)
				setState();
		}
		function onAltNavEnabledChanged()
		{
			tagRoot.visible = altNavigationModel.altNavEnabled;
			if (altNavigationModel.altNavEnabled)
				setState();
		}
	}


	function setState()
	{
		var tag = altNavigationModel.getTagString(this);
		var altNavInput = altNavigationModel.currentAltNavInput;

		//partial match
		if (tag.length > 0 && tag.slice(0, altNavInput.length) === altNavInput)
		{
			//exact match perform actions
			if (altNavInput.length === tag.length)
			{
				matchActionFunc();
				altNavigationModel.altNavEnabled = false;
			}
			else
			{
				tagText.text = tag.slice(altNavInput.length);
				return;
			}
		}
		tagRoot.visible = false;
	}


	Text
	{
		id:							tagText
		color:						"white"
		font:						jaspTheme.fontLabel
		anchors.centerIn:			parent
		horizontalAlignment:		Text.AlignHCenter
		verticalAlignment:			Text.AlignVCenter
	}

}
