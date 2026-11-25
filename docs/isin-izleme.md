# Işın izleme: yapı oluşturucu ve yürüyücüler

Yeni çıkmış bir oyun, ışın izlemeyi (gölgeler, dolaylı ışık) donanım
API'si yerine compute programlarıyla yapar: ayrı bir kütüphanedeki **yapı
oluşturucu** (yaklaşık 50 compute programı) BVH yapılarını title belleğine
yazar; oyunun kendi **yürüyücü** programları (gölge ışını izleme) bu yapıları
adresle okuyarak gezer. Bu belge, bunları PC'de çalıştırmak için öğrenilenleri
toplar.

## Yürüyücüler descriptor'ı yalnızca adres için yükler

Yürüyücü, bindless kaynak yığınından (SM 6.6 `ResourceDescriptorHeap`
karşılığı) hesapladığı bir indeksle 4 dword'lük bir tampon descriptor'ı
yükler, ama onu hiçbir kaynak komutunda kullanmaz: yalnızca taban adresini
maskeler (`d0 & 0xffffff00`, `d1 & 0xff`) ve o adresten bellek okur.

Bir PC katmanında descriptor'lar title belleğinde konsol biçiminde durmaz
(API çağrılarıyla yazılan host kayıtlarıdır); dolayısıyla adres shader'ın
okuyabileceği bir yerde değildir. Çözüm: host, yığının her yuvası için o
yuvadaki tampon görünümünün title adresini tutan bir tablo (yuva başına iki
dword) besler; yuva yazıldığı anda güncellenir. Shader, yalnızca adres olarak
kullanılan yığın descriptor'ı yüklemesini bu tablodan bir okumaya çevirir.
Sonrası, title belleğini adresle okuyan genel yoldur (64 KiB sayfa dizini +
host kopyaları + eksik sayfa bildirimi).

## Yürüyücüler oluşturucu olmadan çalışmamalı

Oluşturucu çalışmıyorsa yürüyücünün gezdiği bellek bir yapı değildir;
yürüyüş bitmeyebilir ve GPU kilitlenir. Yürüyücüler yalnızca oluşturucunun
tüm programları çalışırken çalıştırılır.

## Sabitler adres değildir

Oluşturucu yapı adreslerini kök sabitleriyle (ya iki dword: alt 32 bit + üst
8 bit, ya da 256 bayt birimiyle tek dword) verir. Bunu tahmin eden bir kural
yalnızca oluşturucunun programlarına uygulanmalı: yürüyücülerin sabitleri
çoğunlukla float'tır ve neredeyse her pozitif float 256 bayt birimli bir
adrese benzer.

## Açık sorun: oluşturucunun dokunduğu bellek

Oluşturucu her karede yüzlerce dispatch ile binlerce ayrı title tamponuna
(her mesh'in alt yapısı) dokunur. Her tampon için bir host kopyası açıp onu
çalışma boyunca tutmak ve her komut listesinde hepsini tazelemek, kopya
numaralarını (ör. 1023) ilk saniyelerde tüketir ve kare başına gigabaytlarca
kopya kaydeder; komut ayırıcıları bellek dışı kalır. Gereken tasarım:

- kopya numaralarının geri dönüşümü (son karelerde kullanılmayanı bırak,
  sayfa dizinindeki girdilerini sıfırla),
- tazelemeyi yalnızca dispatch'in adlandırdığı ya da eksik bildirdiği
  kopyalarla sınırlama,
- shader'ın yazdığı sayfalar için kirli-sayfa maskesi (geri kopyalamayı
  sınırlamak için).
