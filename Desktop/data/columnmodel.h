
#ifndef COLUMN_MODEL_H
#define COLUMN_MODEL_H


#include <QIdentityProxyModel>
#include "columntype.h"
#include "undostack.h"
#include <QTimer>

struct ColumnInfo;	///< dataset.h — the lane schema's column record (NEO adapter reads)

class DataSet;

/// 
/// This pipes through the label-information for a single column from DataSetPackage
/// The column is selected by changing `proxyParentColumn` from DataSetTableProxy
class ColumnModel : public QIdentityProxyModel
{
	Q_OBJECT

	// NEO adapter (R2 step 3): schema-backed reads for lane datasets. The legacy mirror is
	// grow-only — it never renames — so binding the editor's fields to `column.*` served
	// stale names after NEO renames (the edit "didn't stick"). These properties serve
	// ColumnInfo when the dataset isOpen(), the legacy Column otherwise, and rebind below.
	Q_PROPERTY(QString		columnName					READ columnNameQ														NOTIFY chosenColumnChanged		)
	Q_PROPERTY(QString		columnTitle					READ columnTitle													NOTIFY columnTitleChanged	)
	Q_PROPERTY(QString		columnDescription			READ columnDescription													NOTIFY columnDescriptionChanged)
	// The excision, Cut 7: hasLabels/filteredOut/rowWidth/valueMaxWidth/labelMaxWidth/dropLevels
	// Q_PROPERTYs died with the label editor QML (B2 rebuilds on the jasp:labels overlay).
	Q_PROPERTY(int			chosenColumn				READ chosenColumn				WRITE setChosenColumn			NOTIFY chosenColumnChanged			)
    Q_PROPERTY(bool			visible						READ visible                    WRITE setVisible                NOTIFY visibleChanged				)
	Q_PROPERTY(bool			columnIsFiltered			READ columnIsFiltered													NOTIFY columnIsFilteredChanged		)
	Q_PROPERTY(bool			nameEditable				READ nameEditable													NOTIFY nameEditableChanged			)
	Q_PROPERTY(QString		computedType				READ computedType													NOTIFY computedTypeChanged			)
	Q_PROPERTY(bool			computedTypeEditable		READ computedTypeEditable													NOTIFY computedTypeEditableChanged	)
	Q_PROPERTY(QVariantList	computedTypeValues			READ computedTypeValues													NOTIFY computedTypeValuesChanged	)
	Q_PROPERTY(QString		currentColumnType			READ currentColumnType			WRITE setColumnType				NOTIFY columnTypeChanged				)
	Q_PROPERTY(QVariantList	columnTypeValues			READ columnTypeValues													NOTIFY columnTypeValuesChanged	)
	Q_PROPERTY(QVariantList	tabs						READ tabs														NOTIFY tabsChanged					)
    Q_PROPERTY(bool         isVirtual					READ isVirtual														NOTIFY isVirtualChanged				)
    Q_PROPERTY(bool			compactMode					READ compactMode                WRITE setCompactMode            NOTIFY compactModeChanged				)
	Q_PROPERTY(int			rowsTotal					READ rowsTotal														NOTIFY rowsTotalChanged				)
	
	

public:
	ColumnModel();
	
	ColumnModel(const ColumnModel &) = delete;
	ColumnModel(ColumnModel &&) = delete;
	ColumnModel &operator=(const ColumnModel &) = delete;
	ColumnModel &operator=(ColumnModel &&) = delete;
	static QVariant columnTypeFriendlyMapping(computedColumnType compColT);
	
	QString			columnNameQ();
	QString			columnTitle()				const;
	QString			columnDescription()				const;
	QString			computedType()				const;
	bool			computedTypeEditable()			const;
	bool			isComputed()				const;
	QVariantList	computedTypeValues()			const;
	QString			currentColumnType()			const;
	QVariantList	columnTypeValues()			const;
	int				rowsTotal()						const;
	// The excision, Cut 7: dropLevels/autoSort/computeFilter reads died with the label editor
	// QML (drop/keep never prunes the lane dictionary; autosort and compute filters return
	// as derivations / the jasp:labels overlay, B2).


	bool			setData(const QModelIndex & index, const QVariant & value,	int role = Qt::EditRole)			override;
	QVariant		data(	const QModelIndex & index,							int role = Qt::DisplayRole)	const	override;
	QVariant		headerData(int section, Qt::Orientation orientation, int role)							const	override;
	int				rowCount(const QModelIndex & parent = QModelIndex())								const	override;
	//int				columnCount(const QModelIndex & = QModelIndex())										const	override;

	bool			visible()			const {	return _visible; }
	int				chosenColumn()		const;
	bool			nameEditable()		const;
	
	// The excision, Cut 5: reverse/reverseValues/toggleAutoSortByValues/moveSelection·Up·Down/
	// setChecked/setValue/setLabel/deleteLabel/addLabel/add·removeEmptyValue/setUseCustom·
	// EmptyValues/hasSeveralNumericValues + column() died with Column (the label editor
	// returns in B2).
	// The excision, Cut 7: with the label editor QML gone, the label-editor survivors died
	// too — resetFilterAllows/resetEmptyValues/unselectAll/filteredOut/rowWidth-family/
	// setSelected/removeAllSelected/getSortedSelection and the whole selection set.
	Q_INVOKABLE void undo()			{ if (undoStack()) undoStack()->undo(); }
	Q_INVOKABLE void redo()			{ if (undoStack()) undoStack()->redo(); }
	// The excision, Cut 7: isColumnNameFree died with CreateComputeColumnDialog (DataSet's
	// schema predicate is the truth for the rename flows that return).
	
	UndoStack *	undoStack();

	void setColumnTitle(			const QString &		newColumnTitle);
	void setColumnDescription(	const QString &		newColumnDescription);
	void setColumnType(			QString				type);
	
	Q_INVOKABLE void setColumnTitleQ(			const QString &		newColumnTitle)				{ setColumnTitle(newColumnTitle);				}
	Q_INVOKABLE void setColumnDescriptionQ(		const QString &		newColumnDescription)			{ setColumnDescription(newColumnDescription);	}
	Q_INVOKABLE void setColumnNameByQString(	const QString &		newColumnName)			{ setColumnNameQ(newColumnName);				}
	// The excision, Cut 7: setHasLabelsQ/setAutoSortQ/setComputeFilterQ/setDropLevelsQ died
	// with the label editor QML; setComputedType too (computedType is read-only until
	// derivations; currentColumnType's setColumnType is the live retype path).

	QVariantList tabs()	const;

	bool columnIsFiltered() const;
	bool isVirtual()		const { return _virtual; }
	bool compactMode()		const;
	
	// The excision, Cut 7: hasLabels/setHasLabels died with the labelsView QML (B2).
	
public slots:
	void 		setVisible(bool visible);
	void 		setChosenColumn(int chosenColumn);
	void 		setChosenColumnByName(const QString chosenName, int colIndex=-1);
	void 		setColumnNameQ(QString newColumnName);
	void 		refresh();
	void 		checkRemovedColumns(int columnIndex, int count);
	void 		checkInsertedColumns(const QModelIndex & parent, int first, int last);
	void 		checkCurrentColumn( int dataSetId, QStringList changedColumns, QStringList missingColumns, QMap<QString, QString>	changeNameColumns, bool rowCountChanged, bool hasNewColumns);
	void 		shownDataSetChangedHandler(DataSet * newDataSet);
	void 		setCompactMode(bool newCompactMode);
	void 		languageChangedHandler();

signals:
	void 		visibleChanged(bool visible);
	void 		columnNameChanged();
	void 		columnDescriptionChanged();
	void 		chosenColumnChanged();
	void 		columnTitleChanged();
	void 		computedTypeChanged();
	void 		isComputedChanged();
	void 		computedTypeEditableChanged();
	void 		computedTypeValuesChanged();
	void 		columnTypeValuesChanged();
	void 		columnTypeChanged();
	void 		columnIsFilteredChanged();
	void 		beforeChangingColumn(QString chosenName);
	void 		nameEditableChanged();
	void 		tabsChanged();
	void 		rowsTotalChanged();
	void 		isVirtualChanged();
	void 		compactModeChanged();
	// The excision, Cut 7: filteredOutChanged/rowWidthChanged/dropLevelsChanged/
	// valueMaxWidthChanged/labelMaxWidthChanged/hasLabelsChanged/allFiltersReset/
	// emptyValuesChanged/autoSortChanged/computeFilterChanged died with the label editor QML.
	QString 	columnNameForIndex(int index);

	
private:
	// The excision, Cut 7: getSortedSelection/setValueMaxWidth and the _selected/_lastSelected/
	// _valueMaxWidth/_labelMaxWidth/_rowWidth members died with the label editor QML.
	void					clearVirtual();
	// NEO adapter: the chosen column's ColumnInfo when the shown dataset is lane-bound
	// (nullptr otherwise, or when nothing valid is chosen). The SCHEMA is the truth for
	// name/display/type — the mirror's names go stale after NEO renames (grow-only).
	const ColumnInfo *	laneSchemaColumn() const;
	// The shown dataset's schemaChanged (a lane edit landed): refresh the editor's reads.
	void					laneSchemaRefreshed();
	// Fires the notify signals of the (GUI-side) properties that depend on the chosen column,
	// as well as chosenColumnChanged which drives the `column` Q_PROPERTY.
	void					notifyColumnChanged();

	struct
	{
		QString				name, title, description;
		columnType			type = columnType::scale;
		computedColumnType	computedType = computedColumnType::notComputed;
	} _dummyColumn;	// The excision, Cut 7: the computeFilter field died with the computed-column editor

	bool					_visible			= false,
								_editing		= false,
								_virtual		= false,
								_compactMode		= false,
								_beingRefreshed		= false;
	// The excision, Cut 7: the `Column * _column` member died at last — it survived Cut 5
	// only because a stale `class Column;` fwd-decl in undostack.h kept it compiling.
	DataSet					*_shownDataSet		= nullptr;
	int						_columnIndex		= -1;
};

#endif // COLUMN_MODEL_H
