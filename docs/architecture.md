# aura — mimari

Tasarım öncelikleri, sırasıyla: **bellek ayak izi**, **kare verimi**,
doğruluk. Aşağıdaki her alt sistemin nedeni, başvuru katmanının 16 GB RAM'li
test makinesindeki profilidir: yeni çıkmış oyunun dünya kareleri 0,5–15 sn
sürüyordu. Nedenleri sayfalama (süreç 23–25 GB), eşzamanlı GPU gönderimi ve
sayfalanmış belleğe istek başına DirectStorage yazımıydı.

## 1. Oyun belleğinin sahibi biziz, kopyası yok

Başvuru katmanı GPU kaynaklarının sistem belleğinde bir gölgesini tutuyor,
değişiklikleri bulmak için özetini (hash) çıkarıyor, sonra yüklüyor. Bunun
bedeli gigabaytlarca commit ve kare başına onlarca milisaniye.

aura bunun yerine:

- Oyunun adres alanını host'un izin verdiği ölçüde eşler: host GPU'nun
  düzeni kabul ettiği arabellek ve dokular için bir grafik heap'i, etmediği
  yerde düz bir rezervasyon ve hedefli yükleme.
- Yazmaları yalnızca sayfa korumasıyla (write-watch) algılar. Özet çıkarma,
  takma adlı görünümler için bir yedektir ve her zaman artımlıdır: her kare
  dönen bir dilim taranır; sürekli değişen bölgeler (ör. video oynatma
  hedefleri) duruluncaya kadar tam taramaya yükseltilir.
- Bir oyun kaynağının CPU tarafında ikinci, tam boy bir gölgesini asla tutmaz.

Bütçe: katmanın kendi commit'i, sahip olması gereken D3D12 heap'leri dışında
~1 GB'ın altında kalır.

## 2. Eşzamansız GPU gönderimi

Gönderen iş parçacığı gönderimleri hiç beklemez. Bir aktarma iş parçacığı
biten uçuşları kapatır ve kaynaklarını bırakır. Oyunun istediği beklemeler
(fence'ler, bellek değeri beklemeleri) her çağrıda sayfa tablosu yoklanarak
değil, GPU ilerlemesinden yanıtlanır.

## 3. DirectStorage yazım yolu

- `PAGE_DESTINATION` istekleri için kalıcı sayfa pencereleri: her istekte yeni
  bir görünüm yerine bir kez eşlenir, yeniden kullanılır.
- Çalışma kümesi baskı altındayken yazılar, oyun belleğine dokunmadan önce
  commit edilmiş (sayfalanmayan) bir ara tampona iner.
- Çözme (zlib/BCPack) tüm çekirdeklere paralel; öncelikler ağırlıklı
  round-robin ile.

## 4. Shader'lar

Konsol GPU kodu, pipeline kütüphanesi izin verdiğinde önceden, vermediğinde
ilk kullanımda DXIL'e çevrilir ve diske önbelleklenir. Çeviri hataları
pipeline başına sayılır ve raporlanır; hiçbir draw, shader'ını adlandıran bir
istatistik olmadan sessizce düşürülmez.

Bilinen gereksinimler: grafik aşamalarında giriş SGPR ABI'si (kök düzeninin
dışında okunan SGPR'lar, motorun gizli öneki), konsolun yonga üstü GS
modelinin mesh shader'a eşlenmesi, kontrol akışıyla seçilen descriptor'lar,
veri olarak okunan descriptor'lar.

## 5. Saatler

Oyun zamanı performans sayacından okur. Çizilen kareler uzadığında oyuna
gösterilen sayaç yavaşlar; böylece bir kare oyuna hiçbir zaman ayarlanabilir
bir tavandan (varsayılan 100 ms) uzun görünmez. Bu, dünya yavaşken oyunun akış
(streaming) dengesini korur. Katmanın tempo ve sunum için kullandığı kendi
saati bundan etkilenmez.

## 6. Yapı

Yükleyici, oyunun x86-64 kodunu değiştirmeden süreç içinde çalıştırır; HLE
crate'leri altındaki konsol işletim sistemi yüzeyini sağlar. İçe aktarma
çözüm sırası: bizim modülümüz, ilke izin veriyorsa host fonksiyonu, oyunla
gelen bir DLL, son olarak çağrıldığında adını söyleyen bir taslak.

## Şimdilik bilerek kapsam dışı

Işın izlemeli aydınlatma, HDR çıkış, ağ üzerinden oyun. Oyun tasarım gereği
çevrimdışı çalışır.
