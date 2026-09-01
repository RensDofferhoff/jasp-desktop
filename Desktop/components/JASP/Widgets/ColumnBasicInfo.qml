//
// Copyright (C) 2013-2023 University of Amsterdam
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public
// License along with this program.  If not, see
// <http://www.gnu.org/licenses/>.
//

import QtQuick
import JASP
import JASP.Widgets
import JASP.Controls
import QtQuick.Controls as QTC
import QtQuick.Layouts
Item
{
	id:							common
	implicitHeight:				Math.max(leftColumn.childrenRect.height, rightColumn.childrenRect.height) + 2 * jaspTheme.generalAnchorMargin
	height:						implicitHeight
	property bool showIcons:	true
	property alias columnNameValue:			columnNameVariablesWindow.value
	property alias columnTitleValue:			columnTitleVariablesWindow.value
	property alias columnDescriptionValue:	columnDescriptionVariablesWindow.text
	property alias columnComputedTypeValue:	computedTypeVariableWindow.value
	property alias columnTypeValue:			columnTypeVariableWindow.value
	// Per-field "the user actually typed" flags (set by textEdited, cleared on commit) —
	// the switch-time force-commit commits only these (see VariablesWindow).
	property alias columnNameEdited:		columnNameVariablesWindow.userEditedName
	property alias columnTitleEdited:		columnTitleVariablesWindow.userEditedTitle

	function focusOnTheNamePlease()
	{
		if (visible && columnModel.isVirtual)
			columnNameVariablesWindow.forceActiveFocus()
	}
	
	Connections
	{
		target: columnModel
		function onVisibleChanged()			{ focusOnTheNamePlease(); }
		function onChosenColumnChanged()	{ focusOnTheNamePlease(); }
		function onIsVirtualChanged()		{ focusOnTheNamePlease(); }
	}
	


	Column
	{
		id:			leftColumn
		width:		Math.max(columnTypeVariableWindow.implicitWidth, computedTypeVariableWindow.implicitWidth, columnNameVariablesWindow.implicitWidth)
		spacing:	jaspTheme.rowGroupSpacing

		anchors
		{
			top:		common.top
			left:		common.left
			bottom:		common.bottom
			margins:	jaspTheme.generalAnchorMargin
		}

		property int labelWidth:	Math.max(columnTypeVariableWindow.controlLabel.implicitWidth, computedTypeVariableWindow.controlLabel.implicitWidth, columnNameVariablesWindow.controlLabel.implicitWidth)

		RowLayout
		{
			height:				longNameRow.height

			TextField
			{
				id:					columnNameVariablesWindow
				placeholderText:	qsTr("<First fill in the column name>")
				value:				columnModel.columnName
				// Phantom-rename guard: editingFinished also fires on plain FOCUS-OUT — e.g.
				// clicking another column — with the PREVIOUS column's text still in the field,
				// which renamed the newly chosen column to the previous one's name. Only a real
				// user edit (textEdited fires per keystroke) may submit.
				property bool	userEditedName:	false
				onTextEdited:		userEditedName = true
				onEditingFinished:
				{
					if (userEditedName && columnModel.columnName !== displayValue)
						columnModel.setColumnNameQ(displayValue)
					userEditedName = false
				}
				undoModel:			columnModel
				editable:           columnModel.nameEditable
				label:				qsTr("Name: ")
				controlLabel.width:	leftColumn.labelWidth
			}
		}


		DropDown
		{
			id: columnTypeVariableWindow

			label:				qsTr("Column type: ")
			isBound:			false
			showVariableTypeIcon: true
			values:				columnModel.columnTypeValues
			currentValue:		columnModel.currentColumnType
			onValueChanged:		columnModel.currentColumnType = currentValue
			fieldWidth:			200 * jaspTheme.uiScale
			controlLabel.width:	leftColumn.labelWidth
			enabled:			!columnModel.isVirtual

		}

		DropDown
		{
			id: computedTypeVariableWindow

			label:				qsTr("Computed type: ")
			values:				columnModel.computedTypeValues
			currentValue:		columnModel.computedType
			onValueChanged:		columnModel.computedType = currentValue
			fieldWidth:			200 * jaspTheme.uiScale

			controlLabel.width:	leftColumn.labelWidth
			enabled:			!columnModel.isVirtual && columnModel.computedTypeEditable
		}

		Item
		{
			implicitWidth:			parent.width
			implicitHeight:			showAnalysisButton.height
			visible:				false	// The excision, Cut 5: "show parent analysis" rode the legacy Column (computed-column era returns as derivations)

			RoundedButton
			{
				id:					showAnalysisButton
				text:				qsTr("Show parent analysis")
				width:				parent.width - x
				x:					leftColumn.labelWidth
				onClicked:			{} // The excision, Cut 5: rode the legacy Column (parent-analysis computed columns return as derivations)
			}
		}
	}

	Column
	{
		id:			rightColumn
		spacing:	jaspTheme.rowGroupSpacing

		anchors
		{
			top:		common.top
			left:		leftColumn.right
			right:		common.right
			bottom:		common.bottom
			margins:	jaspTheme.generalAnchorMargin
		}

		property int labelWidth:	Math.max(columnTitleVariablesWindow.controlLabel.implicitWidth, descriptionLabel.implicitWidth)

		RowLayout
		{
			id:					longNameRow
			width:				parent.width


			TextField
			{
				id:					columnTitleVariablesWindow
				label:				qsTr("Long name: ")
				placeholderText:	qsTr("<Fill in a more descriptive name of the column>")
				fieldWidth:			longNameRow.width - ( rightColumn.labelWidth + closeButton.width + helpButton.width)
				value:				columnModel.columnTitle
				// Phantom-rename guard — same as the Name field above.
				property bool	userEditedTitle:	false
				onTextEdited:			userEditedTitle = true
				onEditingFinished:
				{
					if (userEditedTitle && columnModel.columnTitle !== displayValue)
						columnModel.setColumnTitleQ(displayValue)
					userEditedTitle = false
				}
				undoModel:			columnModel
				controlLabel.width:	rightColumn.labelWidth
				enabled:			!columnModel.isVirtual
			}

			Row
			{
				HelpButton
				{
					id:					helpButton
					height:				common.showIcons ? 33 * jaspTheme.uiScale : 0
					width:				height
					visible:			common.showIcons
					helpMD:				allHelp.variableslabeleditorhelp
					buttonPadding:		6 * preferencesModel.uiScale
				}

				MenuButton
				{
					id:					closeButton
					height:				33 * jaspTheme.uiScale
					width:				common.showIcons ? height : 0
					iconSource:			jaspTheme.iconPath + "collapse.png"
					onClicked:			{ columnModel.visible = false }
					toolTip:			qsTr("Close variable window")
					radius:				height
					visible:			common.showIcons
				}
			}
		}

		Item
		{
			id:					descriptionRow
			width:				parent.width
			height:				Math.max(descriptionLabel.height, columnDescriptionVariablesWindow.height)
			enabled:			!columnModel.isVirtual

			Label
			{
				id:				descriptionLabel
				text:			qsTr("Description: ")
				width:			rightColumn.labelWidth
			}

			TextArea
			{
				id:					columnDescriptionVariablesWindow
				height:				implicitHeight
				width:				implicitWidth
				x:					rightColumn.labelWidth + jaspTheme.labelSpacing //Cause that happens inside TextField between labelRect and actual control
				implicitHeight:		columnTypeVariableWindow.height + computedTypeVariableWindow.height + rightColumn.spacing
				implicitWidth:		descriptionRow.width - x
				control.padding:	3 * jaspTheme.uiScale

				text:				columnModel.columnDescription
				wrapMode:           TextEdit.Wrap
				onEditingFinished: 	if(columnModel.columnDescription !== text) columnModel.setColumnDescriptionQ(text)
				applyScriptInfo:	""
				placeholderText:	"..."
				undoModel:			columnModel
				useTabAsSpaces:		false

			}
		}
	}


}
