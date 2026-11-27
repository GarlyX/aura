# aura çalışma zamanı

aura'nın oyunu çalıştıran katmanı: yükleyici, konsol çalışma zamanı
servisleri, Direct3D 12 arka ucu ve shader yeniden derleyicisi. Yeni çıkmış
bir oyunun Xbox Series sürümünü, oyun dosyalarını kendin sağlayarak Windows'ta
çalıştırır. **NVIDIA ekran kartları** için ayarlanmıştır (32 şeritli dalga,
kart belleğinde tamponlar, ayrı iş parçacığında geri kopya).

**Bu depoda hiçbir oyun dosyası, kodu ya da varlığı yoktur.** Oyunun kendi
kopyana ihtiyacın var.

## Durum (2026-11-26)

| | |
|---|---|
| Çalışıyor | Açılış, menüler, oynanışa giriş (hücre/avlu sahnesi), kayıt |
| Hız | Oynanışta ~3 fps (RTX 3080 Ti 12 GB, 16 GB RAM) |
| Eksik | Işın izleme (yapı oluşturucu çevriliyor, oyunda kararsız), oynanabilir hız |

## Derleme

```powershell
cd runtime
$env:AURA_BCPACK_TABLES = "C:\yol\bcpack-tables.bin"   # ya da local\bcpack-tables.bin
cargo build --profile diag -p victory --features diagnostics
```

BCPack biçim tabloları oyunla ya da VIctory'nin kaynağıyla gelir, bu depoya
girmez (`docs/asset-policy.md`).

## Oynama

```powershell
target\diag\aura-run.exe run "D:\oyun\oyun.exe" --execute
```

Ayarlar ortam değişkenleriyle (`VICTORY_*`, tarihî adlarıyla) yapılır;
önemli olanlar kaynak yorumlarında belgelidir.

## Teşekkür ve lisans

GPL-3.0-or-later. Bu katman sıfırdan değil, şu projelerin üzerine kurulur:

- **[VIctory](https://github.com/Marvisak/VIctory)** — Marvisak ve katkıcıları:
  yükleyici, çalışma zamanı servisleri, D3D12 arka ucu, shader yeniden derleyicisi.
- **victory-fork** — bu katmanın doğrudan atası (eski README: `README.upstream.md`).
- **[DexNom RayTime](https://github.com/DexNom/VIctory)** — AMD odaklı çatal;
  ışın izleme ve bellek dizini fikirleri için başvuru (GPL-3.0).
- **[XCL](https://github.com/xcl-dev/xcl)** — davranış karşılaştırması için başvuru.

aura'ya özgü değişiklikler [Claude Code](https://claude.com/claude-code) ile geliştirildi.
