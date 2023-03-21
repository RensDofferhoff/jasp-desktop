#include "transceiver.h"
#include "mqtt_transceiver.h"
#include "json/json.h"
#include "log.h"

Transceiver* Transceiver::_instance = nullptr;

Transceiver* Transceiver::getInstance()
{
	if (!_instance)
		_instance = new MqttTransceiver(nullptr);
	return _instance;
}
