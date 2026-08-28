#include "log.h"
#include <QThread>
#include "dataenums.h"
#include "datasetbasenode.h"
#include <QGuiApplication>

DataSetBaseNode::DataSetBaseNode(dataSetBaseNodeType typeNode, QObject * parent) 
	: QAbstractTableModel(nullptr), _type(typeNode), _nodeAbove(dynamic_cast<DataSetBaseNode *>(parent))
{
	if(QGuiApplication::instance())
		this->moveToThread(QGuiApplication::instance()->thread());
	else if(parent && parent->thread() != QThread::currentThread())
		this->moveToThread(parent->thread());

	setParent(parent);
	
	if(_nodeAbove)
		_nodeAbove->registerNode(this);
	
	connect(this, &QAbstractItemModel::modelReset,		this,	&DataSetBaseNode::rowCountChanged);
	connect(this, &QAbstractItemModel::rowsInserted,	this,	&DataSetBaseNode::rowCountChanged);
	connect(this, &QAbstractItemModel::rowsRemoved,		this,	&DataSetBaseNode::rowCountChanged);

	connect(this, &QAbstractItemModel::modelReset,		this,	&DataSetBaseNode::columnCountChanged);	
	connect(this, &QAbstractItemModel::columnsInserted,	this,	&DataSetBaseNode::columnCountChanged);
	connect(this, &QAbstractItemModel::columnsRemoved,	this,	&DataSetBaseNode::columnCountChanged);
}

DataSetBaseNode::~DataSetBaseNode()
{
	if(_nodeAbove)
		_nodeAbove->unregisterNode(this);
	 
	_nodeAbove = nullptr;
}

void DataSetBaseNode::registerNode(DataSetBaseNode *child)
{
	_nodesBelow.insert(child);
}

void DataSetBaseNode::unregisterNode(DataSetBaseNode *child)
{
	child->_nodeAbove = nullptr;
	_nodesBelow.erase(child);
}

bool DataSetBaseNode::nodeStillExists(DataSetBaseNode *node) const
{
	if(node == this)
		return true;
	
	for(DataSetBaseNode * child : _nodesBelow)
		if(child->nodeStillExists(node))
			return true;

	return false;
}

void DataSetBaseNode::incRevision()
{
	_revision++;
	checkForChanges();
}

int DataSetBaseNode::nestedRevision()
{
	int rev = _revision;
	
	//Sum (not product) of child revisions: a multiplier is fragile as a change detector because any
	//child whose revision is 0 forces the whole product to 0, masking parent-only changes. A
	//monotonic sum strictly increases whenever the node or any descendant's revision is incremented.
	for(DataSetBaseNode * child : _nodesBelow)
		rev += child->nestedRevision();
	
	return rev;
}

void DataSetBaseNode::checkForChanges()
{
	if(_nodeAbove)
		_nodeAbove->checkForChanges();
	else
	{
		int nested = nestedRevision();
		
		if(nested != _previousNestedRevision)
			emit somethingModified();
		
		_previousNestedRevision = nested;
	}
}

QHash<int, QByteArray> DataSetBaseNode::roleNames() const
{
	static bool						set = false;
	static QHash<int, QByteArray> roles = QAbstractTableModel::roleNames();

	if(!set)
	{
		for(const auto & enumString : dataPkgRolesToStringMap())
			roles[int(enumString.first)] = QString::fromStdString(enumString.second).toUtf8();

		set = true;
	}

	return roles;
}
