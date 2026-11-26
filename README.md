# aura

**aura**, yeni çıkmış bir oyunun Xbox Series sürümünü, oyun dosyalarını
kendin sağlayarak Windows PC'de yerel olarak çalıştırmayı hedefleyen bir
uyumluluk katmanıdır. Sıfırdan yazılmaktadır.

**Bu depoda hiçbir oyun dosyası, kodu ya da varlığı yoktur.** Oyunun kendi
kopyana ihtiyacın var. Proje, oyunun geliştiricisi ya da yayıncısıyla bağlantılı
değildir ve onlar tarafından desteklenmez.

## Durum (2026-11-25)

| | |
|---|---|
| Hedef | Oyunu baştan sona, oynanabilir hızda (≥30 fps) ve eksiksiz görüntüyle açmak |
| Bugün | Oyunu açıyor ve oynanışa giriyor (`runtime/`, NVIDIA için ayarlı); oynanışta ~2-3 fps, ışın izleme deneysel |
| Bitenler | Başlık belleği modülü (tek bölüm, iki adres; yazma izleme + artımlı özet yedeği); shader çevirisinin ilk hattı: konsol GPU kodu çözücüsü, kontrol akışı, SSA, şerit (EXEC) maskeleri, geometri shader'ı → HLSL mesh shader. Oyunun 196 geometri programının tamamı çevriliyor ve DirectX derleyicisinden hatasız geçiyor |
| Sıradaki | Çevrilen shader'ların çalışma zamanı bağlamaları ve oyun içinde doğrulanması; yükleyici, konsol API'leri, D3D12.X → D3D12 |
| Ölçülen hedefler | Katmanın kendi belleği ~1 GB altında; eşzamansız GPU gönderimi; kare başına kopya yok |

Tasarım ve gerekçeler: [`docs/architecture.md`](docs/architecture.md).

## Gereksinimler

- Windows 10/11 x64, AVX2 destekli işlemci
- Shader model 6.6 ve mesh shader destekli bir Direct3D 12 ekran kartı
- Rust (kararlı sürüm)
- Oyunun kendi kopyan (depoya asla eklenmez)

## Derleme

```powershell
cargo build --release
cargo test --workspace
```

## Oynama

Oyunu çalıştıran katman `runtime/` klasöründedir (ayrıntılar `runtime/README.md`):

```powershell
cd runtime
cargo build --profile diag -p victory --features diagnostics
target\diag\aura-run.exe run "D:\oyun\oyun.exe" --execute
```

Işın izleme için deneysel kip: `$env:VICTORY_RAY_TRACING = "static-bvh"` (oyunun hazır
dünya yapıları üzerinden ışın yürüyüşü; dinamik yapı oluşturucu kapalı).

## Kurallar

- Depoya oyuna ait hiçbir dosya, kod, varlık, anahtar ya da oyunun adı girmez.
  Kodda ve belgelerde oyundan "yeni çıkmış bir oyun" diye söz edilir.
- Kişisel bilgiler (yollar, kullanıcı adları, e-postalar, anahtarlar,
  belirteçler) depoya girmez. `.gitignore` oyun dosyası türlerini dışarıda tutar.
- Her kod değişikliği kendi commit'iyle gelir; testler ve `cargo clippy` temiz kalır.
- Yorumlar bir şeyin **neden** öyle olduğunu anlatır.

## Lisans

GPL-3.0-or-later (bkz. `LICENSE`). `runtime/` VIctory (Marvisak) üzerine kuruludur ve
çatallarından (victory-fork, DexNom RayTime) yararlanır; atıflar `runtime/README.md`'de.
`crates/` altındaki aura modülleri sıfırdan yazılmaktadır.
