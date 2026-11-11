//! WinRT BLE GATT backend for the ATVV service, ported unchanged from the
//! hardware-verified `xiaomi-remote-mic` implementation (2026-08-25, real
//! `2717:32B8` device). Enumerates Windows-paired devices exposing the ATVV
//! service, maintains the GATT session, and bridges characteristic
//! notifications into the session loop's channel.

use anyhow::{Context as _, Result, bail};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;
use uuid::Uuid;
use windows::{
    Devices::{
        Bluetooth::{
            BluetoothCacheMode, BluetoothConnectionStatus, BluetoothLEDevice,
            GenericAttributeProfile::{
                GattCharacteristic, GattCharacteristicProperties,
                GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
                GattDeviceService, GattSession, GattSessionStatus, GattWriteOption,
            },
        },
        Enumeration::{DeviceInformation, DeviceInformationCollection},
    },
    Foundation::TypedEventHandler,
    Storage::Streams::{DataReader, DataWriter, IBuffer},
    core::GUID,
};

use crate::atvv::{BATTERY_LEVEL_UUID, MODEL_NUMBER_UUID, SERVICE_UUID};

const BATTERY_SERVICE_UUID: Uuid = uuid::uuid!("0000180f-0000-1000-8000-00805f9b34fb");
const DEVICE_INFORMATION_SERVICE_UUID: Uuid = uuid::uuid!("0000180a-0000-1000-8000-00805f9b34fb");

#[derive(Clone)]
pub(in crate::ble) struct Characteristic {
    inner: GattCharacteristic,
    _service: GattDeviceService,
    pub uuid: Uuid,
    write_without_response: bool,
    notify: bool,
    indicate: bool,
}

impl Characteristic {
    pub(in crate::ble) fn preferred_write_type(&self) -> WriteType {
        if self.write_without_response {
            WriteType::WithoutResponse
        } else {
            WriteType::WithResponse
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::ble) enum WriteType {
    WithResponse,
    WithoutResponse,
}

#[derive(Debug)]
pub(in crate::ble) struct ValueNotification {
    pub uuid: Uuid,
    pub value: Vec<u8>,
}

struct Subscription {
    characteristic: GattCharacteristic,
    token: i64,
}

pub(in crate::ble) struct Peripheral {
    id: String,
    name: String,
    service: GattDeviceService,
    device: BluetoothLEDevice,
    session: GattSession,
    characteristics: RwLock<Vec<Characteristic>>,
    notification_tx: mpsc::UnboundedSender<ValueNotification>,
    notification_rx: Mutex<Option<mpsc::UnboundedReceiver<ValueNotification>>>,
    subscriptions: Mutex<Vec<Subscription>>,
}

impl Peripheral {
    pub(in crate::ble) async fn discover(cached_id: Option<&str>) -> Result<Option<Self>> {
        let selector = GattDeviceService::GetDeviceSelectorFromUuid(to_guid(SERVICE_UUID))
            .context("无法创建 ATVV 服务选择器")?;
        let devices = DeviceInformation::FindAllAsyncAqsFilter(&selector)
            .context("无法开始枚举已配对的 ATVV 设备")?
            .await
            .context("无法枚举已配对的 ATVV 设备")?;
        let Some(info) = select_device(&devices, cached_id)? else {
            return Ok(None);
        };
        let id = info.Id()?.to_string();
        let service = GattDeviceService::FromIdAsync(&info.Id()?)
            .context("无法打开遥控器 ATVV 服务")?
            .await
            .context("Windows 拒绝访问遥控器 ATVV 服务")?;
        let device = service.Device().context("无法读取 ATVV 所属蓝牙设备")?;
        let session = service.Session().context("无法创建 ATVV GATT 会话")?;
        if session.CanMaintainConnection().unwrap_or(false) {
            session
                .SetMaintainConnection(true)
                .context("无法保持 ATVV GATT 连接")?;
        }
        let name = device
            .Name()
            .ok()
            .map(|value| value.to_string())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                info.Name()
                    .map(|value| value.to_string())
                    .unwrap_or_else(|_| "MI RC".into())
            });
        let (notification_tx, notification_rx) = mpsc::unbounded_channel();
        Ok(Some(Self {
            id,
            name,
            service,
            device,
            session,
            characteristics: RwLock::new(Vec::new()),
            notification_tx,
            notification_rx: Mutex::new(Some(notification_rx)),
            subscriptions: Mutex::new(Vec::new()),
        }))
    }

    pub(in crate::ble) fn id(&self) -> &str {
        &self.id
    }

    pub(in crate::ble) fn name(&self) -> &str {
        &self.name
    }

    pub(in crate::ble) async fn connect(&self) -> Result<()> {
        if self.session.CanMaintainConnection().unwrap_or(false) {
            self.session
                .SetMaintainConnection(true)
                .context("无法保持 ATVV GATT 连接")?;
        }
        Ok(())
    }

    pub(in crate::ble) async fn disconnect(&self) -> Result<()> {
        let subscriptions = std::mem::take(&mut *self.subscriptions.lock());
        for subscription in subscriptions {
            let _ = subscription
                .characteristic
                .RemoveValueChanged(subscription.token);
            if let Ok(operation) = subscription
                .characteristic
                .WriteClientCharacteristicConfigurationDescriptorAsync(
                    GattClientCharacteristicConfigurationDescriptorValue::None,
                )
            {
                let _ = operation.await;
            }
        }
        if self.session.CanMaintainConnection().unwrap_or(false) {
            let _ = self.session.SetMaintainConnection(false);
        }
        let _ = self.session.Close();
        let _ = self.service.Close();
        let _ = self.device.Close();
        Ok(())
    }

    pub(in crate::ble) async fn discover_services(&self) -> Result<()> {
        // Always force over-the-air discovery: Windows keeps a persistent
        // GATT cache per bonded device and serves stale attribute handles
        // and CCCD state from it long after the remote has rebooted.
        // Subscribing through cached handles reports success while the
        // remote never actually enables notifications, which muted the
        // ATVV stream until the cache was rebuilt by re-pairing.
        let mut characteristics = query_characteristics(&self.service).await?;
        for (service_uuid, characteristic_uuid) in [
            (DEVICE_INFORMATION_SERVICE_UUID, MODEL_NUMBER_UUID),
            (BATTERY_SERVICE_UUID, BATTERY_LEVEL_UUID),
        ] {
            if let Some(characteristic) = self
                .find_optional_characteristic(service_uuid, characteristic_uuid)
                .await
            {
                characteristics.push(characteristic);
            }
        }
        *self.characteristics.write() = characteristics;
        Ok(())
    }

    async fn find_optional_characteristic(
        &self,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
    ) -> Option<Characteristic> {
        let result = self
            .device
            .GetGattServicesForUuidWithCacheModeAsync(
                to_guid(service_uuid),
                BluetoothCacheMode::Uncached,
            )
            .ok()?
            .await
            .ok()?;
        if result.Status().ok()? != GattCommunicationStatus::Success {
            return None;
        }
        let services: Vec<_> = result.Services().ok()?.into_iter().collect();
        for service in services {
            if let Ok(characteristics) = query_characteristics(&service).await {
                if let Some(characteristic) = characteristics
                    .into_iter()
                    .find(|item| item.uuid == characteristic_uuid)
                {
                    return Some(characteristic);
                }
            }
        }
        None
    }

    pub(in crate::ble) fn characteristics(&self) -> Vec<Characteristic> {
        self.characteristics.read().clone()
    }

    pub(in crate::ble) async fn subscribe(&self, characteristic: &Characteristic) -> Result<()> {
        let descriptor = if characteristic.notify {
            GattClientCharacteristicConfigurationDescriptorValue::Notify
        } else if characteristic.indicate {
            GattClientCharacteristicConfigurationDescriptorValue::Indicate
        } else {
            bail!("GATT 特征 {} 不支持通知", characteristic.uuid);
        };
        let uuid = characteristic.uuid;
        let notifications = self.notification_tx.clone();
        let token = {
            let handler = TypedEventHandler::<
                GattCharacteristic,
                windows::Devices::Bluetooth::GenericAttributeProfile::GattValueChangedEventArgs,
            >::new(move |_, args| {
                if let Ok(args) = args.ok() {
                    match args
                        .CharacteristicValue()
                        .and_then(|value| buffer_bytes(&value))
                    {
                        Ok(value) => {
                            let _ = notifications.send(ValueNotification { uuid, value });
                        }
                        Err(_) => {
                            // A single unreadable notification is dropped;
                            // the ATVV stream self-heals on the next frame.
                        }
                    }
                }
                Ok(())
            });
            characteristic
                .inner
                .ValueChanged(&handler)
                .with_context(|| format!("无法监听 GATT 特征 {}", characteristic.uuid))?
        };
        let status = characteristic
            .inner
            .WriteClientCharacteristicConfigurationDescriptorAsync(descriptor)
            .with_context(|| format!("无法配置 GATT 特征 {} 通知", characteristic.uuid))?
            .await
            .with_context(|| format!("无法启用 GATT 特征 {} 通知", characteristic.uuid))?;
        if status != GattCommunicationStatus::Success {
            let _ = characteristic.inner.RemoveValueChanged(token);
            bail!(
                "启用 GATT 特征 {} 通知失败：{status:?}",
                characteristic.uuid
            );
        }
        self.subscriptions.lock().push(Subscription {
            characteristic: characteristic.inner.clone(),
            token,
        });
        Ok(())
    }

    pub(in crate::ble) fn notifications(&self) -> Result<mpsc::UnboundedReceiver<ValueNotification>> {
        self.notification_rx
            .lock()
            .take()
            .context("GATT 通知接收器已被获取")
    }

    pub(in crate::ble) async fn read(&self, characteristic: &Characteristic) -> Result<Vec<u8>> {
        let result = characteristic
            .inner
            .ReadValueAsync()
            .with_context(|| format!("无法读取 GATT 特征 {}", characteristic.uuid))?
            .await
            .with_context(|| format!("读取 GATT 特征 {} 失败", characteristic.uuid))?;
        if result.Status()? != GattCommunicationStatus::Success {
            bail!(
                "读取 GATT 特征 {} 失败：{:?}",
                characteristic.uuid,
                result.Status()?
            );
        }
        buffer_bytes(&result.Value()?).map_err(Into::into)
    }

    pub(in crate::ble) async fn write(
        &self,
        characteristic: &Characteristic,
        value: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        let writer = DataWriter::new().context("无法创建 GATT 写入缓冲区")?;
        writer.WriteBytes(value).context("无法写入 GATT 缓冲区")?;
        let option = match write_type {
            WriteType::WithResponse => GattWriteOption::WriteWithResponse,
            WriteType::WithoutResponse => GattWriteOption::WriteWithoutResponse,
        };
        let operation = characteristic
            .inner
            .WriteValueWithOptionAsync(
                &writer.DetachBuffer().context("无法完成 GATT 写入缓冲区")?,
                option,
            )
            .with_context(|| format!("无法写入 GATT 特征 {}", characteristic.uuid))?;
        let status = operation
            .await
            .with_context(|| format!("写入 GATT 特征 {} 失败", characteristic.uuid))?;
        if status != GattCommunicationStatus::Success {
            bail!("写入 GATT 特征 {} 失败：{status:?}", characteristic.uuid);
        }
        Ok(())
    }

    pub(in crate::ble) async fn is_connected(&self) -> Result<bool> {
        Ok(
            self.device.ConnectionStatus()? == BluetoothConnectionStatus::Connected
                && self.session.SessionStatus()? == GattSessionStatus::Active,
        )
    }
}

async fn query_characteristics(service: &GattDeviceService) -> Result<Vec<Characteristic>> {
    let result = service
        .GetCharacteristicsWithCacheModeAsync(BluetoothCacheMode::Uncached)
        .context("无法开始枚举 GATT 特征")?
        .await
        .context("无法枚举 GATT 特征")?;
    if result.Status()? != GattCommunicationStatus::Success {
        bail!("枚举 GATT 特征失败：{:?}", result.Status());
    }
    let mut characteristics = Vec::new();
    for inner in result.Characteristics()? {
        let properties = inner.CharacteristicProperties()?;
        characteristics.push(Characteristic {
            uuid: Uuid::from_u128(inner.Uuid()?.to_u128()),
            write_without_response: has_property(
                properties,
                GattCharacteristicProperties::WriteWithoutResponse,
            ),
            notify: has_property(properties, GattCharacteristicProperties::Notify),
            indicate: has_property(properties, GattCharacteristicProperties::Indicate),
            inner,
            _service: service.clone(),
        });
    }
    Ok(characteristics)
}

fn select_device(
    devices: &DeviceInformationCollection,
    cached_id: Option<&str>,
) -> Result<Option<DeviceInformation>> {
    let mut fallback = None;
    for index in 0..devices.Size()? {
        let device = devices.GetAt(index)?;
        let id = device.Id()?;
        if cached_id.is_some_and(|cached_id| id == cached_id) {
            return Ok(Some(device));
        }
        fallback.get_or_insert(device);
    }
    Ok(fallback)
}

fn buffer_bytes(buffer: &IBuffer) -> windows::core::Result<Vec<u8>> {
    let reader = DataReader::FromBuffer(buffer)?;
    let mut value = vec![0; reader.UnconsumedBufferLength()? as usize];
    reader.ReadBytes(&mut value)?;
    Ok(value)
}

fn has_property(
    properties: GattCharacteristicProperties,
    expected: GattCharacteristicProperties,
) -> bool {
    properties.0 & expected.0 != 0
}

fn to_guid(uuid: Uuid) -> GUID {
    GUID::from_u128(uuid.as_u128())
}
