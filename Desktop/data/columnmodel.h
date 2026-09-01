
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
	Q_PROPERTY(QString		columnName					READ columnNameQ												NOTIFY chosenColumnChanged		)
	Q_PROPERTY(QString		columnTitle					READ columnTitle												NOTIFY columnTitleChanged		)
	Q_PROPERTY(QString		columnDescription			READ columnDescription											NOTIFY columnDescriptionChanged)
	Q_PROPERTY(bool			hasLabels					READ hasLabels													NOTIFY hasLabelsChanged		)

    Q_PROPERTY(int			filteredOut					READ filteredOut                                                NOTIFY filteredOutChanged				)
	// The excision, Cut 5: the `column` Q_PROPERTY (a Column*) died with Column.
	Q_PROPERTY(int			chosenColumn				READ chosenColumn				WRITE setChosenColumn			NOTIFY chosenColumnChanged				)
    Q_PROPERTY(bool			visible						READ visible                    WRITE setVisible                NOTIFY visibleChanged					)
	Q_PROPERTY(double		rowWidth					READ rowWidth					WRITE setRowWidth				NOTIFY rowWidthChanged					)
	Q_PROPERTY(double		valueMaxWidth				READ valueMaxWidth												NOTIFY valueMaxWidthChanged				)
	Q_PROPERTY(double		labelMaxWidth				READ labelMaxWidth												NOTIFY labelMaxWidthChanged				)
	Q_PROPERTY(bool			columnIsFiltered			READ columnIsFiltered											NOTIFY columnIsFilteredChanged			)
	Q_PROPERTY(bool			nameEditable				READ nameEditable												NOTIFY nameEditableChanged				)
	Q_PROPERTY(QString		computedType				READ computedType				WRITE setComputedType			NOTIFY computedTypeChanged				)
	Q_PROPERTY(bool			computedTypeEditable		READ computedTypeEditable										NOTIFY computedTypeEditableChanged		)
	Q_PROPERTY(QVariantList	computedTypeValues			READ computedTypeValues											NOTIFY computedTypeValuesChanged		)
	Q_PROPERTY(QString		currentColumnType			READ currentColumnType			WRITE setColumnType				NOTIFY columnTypeChanged				)
	Q_PROPERTY(QVariantList	columnTypeValues			READ columnTypeValues											NOTIFY columnTypeValuesChanged			)
	Q_PROPERTY(QVariantList	tabs						READ tabs														NOTIFY tabsChanged						)
    Q_PROPERTY(bool         isVirtual					READ isVirtual													NOTIFY isVirtualChanged					)
    Q_PROPERTY(bool			compactMode					READ compactMode                WRITE setCompactMode            NOTIFY compactModeChanged				)
	Q_PROPERTY(int			rowsTotal					READ rowsTotal													NOTIFY rowsTotalChanged					)
    Q_PROPERTY(QString		dropLevels					READ dropLevels					WRITE setDropLevels				NOTIFY dropLevelsChanged                )
	
	

public:
	ColumnModel();
	
	ColumnModel(const ColumnModel &) = delete;
	ColumnModel(ColumnModel &&) = delete;
	ColumnModel &operator=(const ColumnModel &) = delete;
	ColumnModel &operator=(ColumnModel &&) = delete;
	static QVariant columnTypeFriendlyMapping(computedColumnType compColT);
	
	QString			columnNameQ();
	QString			columnTitle()					const;
	QString			columnDescription()				const;
	QString			computedType()					const;
	bool			computedTypeEditable()			const;
	bool			isComputed()					const;
	QVariantList	computedTypeValues()			const;
	QString			currentColumnType()				const;
	QVariantList	columnTypeValues()				const;
	int				rowsTotal()						const;
	QString			dropLevels()					const;
	bool			autoSort()						const;
	QString			computeFilter()					const;


	bool			setData(const QModelIndex & index, const QVariant & value,	int role = Qt::EditRole)			override;
	QVariant		data(	const QModelIndex & index,							int role = Qt::DisplayRole)	const	override;
	QVariant		headerData(int section, Qt::Orientation orientation, int role)							const	override;
	int				rowCount(const QModelIndex & parent = QModelIndex())								const	override;
	//int				columnCount(const QModelIndex & = QModelIndex())										const	override;

	bool			visible()			const {	return _visible; }
	int				filteredOut()		const;
	int				chosenColumn()		const;
	bool			nameEditable()		const;
	
	// The excision, Cut 5: reverse/reverseValues/toggleAutoSortByValues/moveSelection·Up·Down/
	// setChecked/setValue/setLabel/deleteLabel/addLabel/add·removeEmptyValue/setUseCustom·
	// EmptyValues/hasSeveralNumericValues + column() died with Column (the label editor
	// returns in B2). resetEmptyValues/resetFilterAllows stay as inert invokables.
	Q_INVOKABLE void resetFilterAllows();
	Q_INVOKABLE void unselectAll();
	Q_INVOKABLE void resetEmptyValues();
	Q_INVOKABLE void undo()				{ if (undoStack()) undoStack()->undo(); }
	Q_INVOKABLE void redo()				{ if (undoStack()) undoStack()->redo(); }
	
	Q_INVOKABLE bool isColumnNameFree(		const QString & name);
	
	UndoStack *	undoStack();

	double rowWidth()			const	{ return _rowWidth;			}
	double valueMaxWidth()		const	{ return _valueMaxWidth;	}
	double labelMaxWidth()		const	{ return _labelMaxWidth;	}

	void setColumnTitle(			const QString &		newColumnTitle);
	void setColumnDescription(		const QString &		newColumnDescription);
	void setComputedType(			QString				computedType);
	void setColumnType(				QString				type);
	
	Q_INVOKABLE void setColumnTitleQ(			const QString &		newColumnTitle)				{ setColumnTitle(newColumnTitle);			}
	Q_INVOKABLE void setColumnDescriptionQ(		const QString &		newColumnDescription)		{ setColumnDescription(newColumnDescription);	}
	Q_INVOKABLE void setColumnNameByQString(	const QString &		newColumnName)				{ setColumnNameQ(newColumnName);				}
	Q_INVOKABLE void setHasLabelsQ(				bool				newHasLabels)				{ setHasLabels(newHasLabels);					}
	Q_INVOKABLE void setAutoSortQ(				bool				newAutoSort)				{ setAutoSort(newAutoSort);					}
	Q_INVOKABLE void setComputeFilterQ(			const QString &		newComputeFilter)			{ setComputeFilter(newComputeFilter);			}
	Q_INVOKABLE void setDropLevelsQ(			QString				dropLevels)					{ setDropLevels(dropLevels);					}
	
	void setUseCustomEmptyValues(	bool				useCustomMissingValues);
	void setCustomEmptyValues(		const QStringList&	customMissingValues);
	void setDropLevels(				QString				dropLevels);
	void setAutoSort(				bool				newAutoSort);
	void setComputeFilter(			const QString &		newComputeFilter);

	QVariantList tabs()		const;

	bool columnIsFiltered() const;
	bool isVirtual()		const	{ return _virtual; }
	bool compactMode()		const;
	
	
	
	bool hasLabels() const;
	void setHasLabels(bool newHasLabels);
	
public slots:
	void 		refreshFilteredOut();
	void 		setVisible(bool visible);
	void 		setChosenColumn(int chosenColumn);
	void 		setChosenColumnByName(const QString chosenName, int colIndex=-1);
	void 		setSelected(int row, int modifier);
	void 		setColumnNameQ(QString newColumnName);
	void 		removeAllSelected();
	void 		setRowWidth(double len);
	void 		refresh();
	void 		checkRemovedColumns(int columnIndex, int count);
	void 		checkInsertedColumns(const QModelIndex & parent, int first, int last);
	void 		openComputedColumn(const QString name);
	void 		checkCurrentColumn( int dataSetId, QStringList changedColumns, QStringList missingColumns, QMap<QString, QString>	changeNameColumns, bool rowCountChanged, bool hasNewColumns);
	void 		shownDataSetChangedHandler(DataSet * newDataSet);
	void 		setCompactMode(bool newCompactMode);
	void 		languageChangedHandler();
	void 		setLabelMaxWidth();

signals:
	void 		visibleChanged(bool visible);
	void 		filteredOutChanged();
	void 		columnNameChanged();
	void 		allFiltersReset();
	void 		rowWidthChanged();
	void 		dropLevelsChanged();
	void 		valueMaxWidthChanged();
	void 		columnDescriptionChanged();
	void 		labelMaxWidthChanged();
	void 		chosenColumnChanged();
	void 		columnTitleChanged();
	void 		computedTypeChanged();
	void 		isComputedChanged();
	void		hasLabelsChanged();
	void 		computedTypeEditableChanged();
	void 		computedTypeValuesChanged();
	void 		columnTypeValuesChanged();
	void 		columnTypeChanged();
	void 		columnIsFilteredChanged();
	void 		beforeChangingColumn(QString chosenName);
	void 		nameEditableChanged();
	void 		tabsChanged();
	void 		emptyValuesChanged();
	void 		rowsTotalChanged();
	void 		isVirtualChanged();
	void 		compactModeChanged();
	void 		autoSortChanged();
	void 		computeFilterChanged();
	QString 	columnNameForIndex(int index);

	
private:
	std::vector<size_t>		getSortedSelection()				const;
	void					setValueMaxWidth();
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
		QString				name, title, description, computeFilter;
		columnType			type = columnType::scale;
		computedColumnType	computedType = computedColumnType::notComputed;
	} _dummyColumn;

	bool					_visible			= false,
							_editing			= false,
							_virtual			= false,
							_compactMode		= false,
							_beingRefreshed		= false;
	double					_valueMaxWidth		= 10,
							_labelMaxWidth		= 10,
							_rowWidth			= 60;
	std::set<QString>		_selected;
	int						_lastSelected		= -1;
	Column				*	_column				= nullptr;
	DataSet				*	_shownDataSet		= nullptr;
	int						_columnIndex		= -1;
};

#endif // COLUMN_MODEL_H
