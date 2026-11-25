# Geometri shader'ları: konsolun yonga üstü GS modeli → mesh shader

Yeni çıkmış oyunun shader paketinde 195 geometri (GS) programı başvuru
katmanında hiç çevrilemiyor. Hepsi aynı aileden: üçgenleri baskın eksene göre
seçip NDC'ye yansıtan bir vokselleştirici. Bu belge aura-shader'ın bu programları
nasıl çevireceğini tanımlar. Bir yama değil, ayrı bir tasarım işidir.

## Konsolda bir GS nasıl çalışır

Konsol GPU'sunda (RDNA2, "NGG"/birleşik aşamalar) GS ayrı bir aşama olarak
çalışmaz. Önceki aşama (ES) verteksleri LDS'e (çalışma grubunun paylaşılan
belleğine) yazar; GS aynı çalışma grubunda LDS'ten okur, ilkel üretir ve
çıktıyı yine LDS + `EMIT`/`CUT` mesajlarıyla verir.

Girişte donanım SGPR'ları şöyle doludur (kullanıcı dword N → s(N+8)):

| SGPR | İçerik |
|---|---|
| s0..s7 | 8 sistem SGPR'ı. s2: LDS dal ofseti bileşeni. s3: wave bilgisi; bit 15:8 = aktif ilkel sayısı (exec maskesi bundan kurulur), bit 23:16 → m0 |
| s8..s15 | kullanıcı dword 0..7: motorun **gizli öneki**. dword 0:1 = 40-bit işaretçi; +0xb8'de LDS halka tabanı |
| s16.. | kök düzeninde beyan edilen kökler (tablo, sabitler, heap'ler) |
| v0 | giriş: verteks indeksleri (16-bit çiftler) |

Önemli tuzak: s3'ün 15:8 bitleri 0 verilirse exec = 0 olur ve shader hiçbir
şey yapmaz. "Bilinmeyen SGPR = 0" kaçışı burada yanlış sonuç verir; sessizce
boş çıktı üretir.

## Çeviri modeli

1. **Aşama eşlemesi:** ES + GS çifti tek bir DXIL mesh shader'ına (amplification
   gerekmez) çevrilir. Bir çalışma grubu = konsoldaki bir GS alt grubu.
2. **LDS:** `groupshared` dizi. ES kısmı vertekslerini yazar, `GroupMemoryBarrier`
   sonrası GS kısmı okur. LDS adresleri (s2 + halka tabanı + indeks) aynı düzende
   korunur, böylece `ds_read*`/`ds_write*` komutları doğrudan çevrilir.
3. **Sistem SGPR'ları:** s3 ve s2, çevirmenin ürettiği bir "aşama girişi"nden
   gelir: aktif ilkel sayısı = çizim çağrısının ilkel sayısının bu gruba düşen
   kısmı. s0..s7'nin geri kalanı modele göre sabittir.
4. **Gizli önek:** dword 0..7 kök düzeninde yok ama motor her çizimde yazıyor.
   aura, D3D12.X tarafında bu sekiz dword'ü komut listesinden yakalar ve kök
   sabitleri olarak bağlar; 40-bit işaretçi oyun belleğindeki bir arabelleğe
   adres olarak çözülür (oyun belleği aura'da zaten GPU'ya görünür).
5. **Çıktı:** `EMIT` → mesh çıktısında bir verteks; `CUT` → şerit sonu.
   Şeritler üçgen listesine açılır. Grup başına en çok verteks/ilkel sayısı,
   programın beyan ettiği üst sınırlardan hesaplanır (`SetMeshOutputCounts`).
6. **Mesajlar:** `s_sendmsg MSG_GS*` (EMIT/CUT/DONE) çevirmenin ara temsilinde
   ayrı düğümler olur; DXIL'de yukarıdaki çıktı yazımlarına dönüşür.

## Doğrulama planı

- Birim: küçük, el yazımı bir GFX10 GS programı (bir üçgen giriş, iki EMIT) →
  DXIL → DXIL doğrulayıcı.
- Sayım: shader paketinin tam taramasında bu ailenin 195 programının tamamı
  çevrilmeli; yeni hata sınıfı çıkmamalı.
- Görsel: vokselleştirme sonucunu kullanan sahne (dolaylı aydınlatma/
  görünürlük) oyun içinde ekran görüntüsüyle karşılaştırılmalı.

## Açık sorular

- ES kısmı GS ile aynı programda mı geliyor, ayrı bir kapsayıcıda mı? İlk
  ölçümler birleşik aşama olduğunu gösteriyor; doğrulanacak.
- Mesh çıktı sınırları (256 verteks / 256 ilkel) aşılırsa grup bölünmeli.
