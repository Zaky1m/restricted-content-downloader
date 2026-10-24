<div dir="rtl">

# التشغيل

[English](deploy.md) · **العربية**

## ما تحتاج إليه

- رمز بوت من [@BotFather](https://t.me/BotFather)
- معرّف التطبيق (API ID) ومفتاحه (API hash) من [my.telegram.org](https://my.telegram.org)
- قاعدة PostgreSQL بالإصدار 14 أو أحدث (إعداد Docker يشغّل واحدة لك)
- حساب تيليجرام أو أكثر لمجموعة التحميل. استخدم حسابات منفصلة لا حسابك الشخصي
- قناة أو مجموعة خاصة لسجل الأخطاء، والبوت مشرف فيها

يحتفظ البوت باتصال مفتوح مع تيليجرام ويستقبل التحديثات عبره، فلا يحتاج إلى عنوان عام أو نطاق أو منفذ مفتوح.

## باستخدام Docker

<div dir="ltr">

```bash
git clone https://github.com/Zaky1m/restricted-content-downloader.git
cd restricted-content-downloader
cp .env.example .env
```

</div>

املأ ملف `.env` (انظر الجدول أدناه)، ثم:

<div dir="ltr">

```bash
docker compose up -d
docker compose run --rm bot rcd pool-login
docker compose logs -f bot
```

</div>

يطلب `pool-login` الرمز الذي يرسله تيليجرام إلى كل رقم في `POOL_PHONES`، ثم يحفظ الجلسات في قاعدة البيانات. ولإضافة حساب آخر لاحقًا شغّل `rcd pool-login --phone +12025550123`.

## على خادم من دون Docker

ثبّت Rust (الإصدار المستقر) وFFmpeg وPostgreSQL، ثم:

<div dir="ltr">

```bash
cargo build --release
cp .env.example .env    # fill in the values
./target/release/rcd check
./target/release/rcd pool-login
./target/release/rcd serve
```

</div>

ولإبقائه يعمل، انسخ [`deploy/rcd.service.example`](../deploy/rcd.service.example) إلى `/etc/systemd/system/rcd.service`، وعدّل المستخدم والمسارات، ثم شغّل `sudo systemctl enable --now rcd`. وعند استقبال `SIGTERM` يوقف البوت مهامه بشكل سليم، وتكمل عند التشغيل التالي.

## الإعدادات

| المتغير | مطلوب | المعنى |
|---|---|---|
| `API_ID`، `API_HASH` | نعم | بيانات تطبيق تيليجرام |
| `BOT_TOKEN` | نعم | الرمز من @BotFather |
| `BOT_USERNAME` | نعم | اسم مستخدم البوت دون `@`، ويُستخدم في روابط الإحالة |
| `OWNER_IDS` | نعم | معرفات المالكين في تيليجرام، مفصولة بفواصل |
| `ADMIN_USERNAME` | نعم | حساب الدعم الذي يظهر للمستخدمين، دون `@` |
| `LOG_CHANNEL_ID` | نعم | المحادثة التي تستقبل سجل الأخطاء |
| `DATABASE_URL` | نعم | رابط PostgreSQL، أو متغيرات `DB_*` المنفصلة |
| `SESSION_ENCRYPTION_KEY` | نعم | 64 حرفًا ست عشريًا، من `openssl rand -hex 32` |
| `POOL_PHONES` | لا | أرقام حسابات التحميل للأمر `pool-login` |
| `CHANNEL_LINK` | لا | رابط زر القناة، ويُخفى الزر إذا كان فارغًا |
| `START_IMAGE`، `ARCHIVE_CHANNEL_ID` | لا | صورة الترحيب في `/start` والقناة التي تُحفظ فيها |
| `POSTGRES_PASSWORD` | مع Docker | كلمة مرور قاعدة البيانات التي يشغّلها compose |
| `DATA_DIR`، `LOCALES_DIR` | لا | القيمتان الافتراضيتان `data` و`locales` |

احفظ `SESSION_ENCRYPTION_KEY` في مكان آمن ولا تغيّره أبدًا، فالجلسات المحفوظة لا تُقرأ من دونه.

## الانتقال من نسخة Python

يقرأ الأمر `rcd import --sessions-dir <folder>` جلسات البوت السابق المبني على Pyrogram (ملفات `.session` وجدول `user_sessions`)، ثم يشفّرها ويحفظها في الجداول الجديدة، فيبقى المستخدمون مسجلين. وبنية قاعدة البيانات مشتركة: اجعل `DATABASE_URL` يشير إلى القاعدة الحالية، وأوقف البوت القديم، وشغّل الاستيراد مرة واحدة قبل `rcd serve`. وتكرار تشغيله آمن.

## التحديث

<div dir="ltr">

```bash
git pull
docker compose up -d --build
```

</div>

تُطبّق ترحيلات قاعدة البيانات عند التشغيل.

</div>
