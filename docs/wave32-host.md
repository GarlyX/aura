# 64 şeritli konsol dalgaları, 32 şeritli host

Konsol GPU'su compute programlarının çoğunu 64 şeritlik dalgalarla (wave64)
çalıştırır. Bazı PC ekran kartları (ör. NVIDIA) yalnızca 32 şerit destekler:
`WaveSize(64)` bildiren bir compute pipeline'ı D3D12'de
`E_INVALIDARG` ile reddedilir ve o program hiç çalışmaz. Bu belge, bir konsol
dalgasını iki host dalgasına (iki "yarı") bölerken hangi durumların güvenli
olduğunu ve neden öyle olduğunu anlatır.

## Yarılar arası takas

Her konsol dalgası iki host dalgasıdır: şeritler 0-31 ve 32-63. Şerit
maskeleri, şerit okumaları (`v_readlane`, `v_readfirstlane`) ve şerit
numaraları (`v_mbcnt`) diğer yarıdan bir şey isteyebilir; bunlar LDS'teki iki
takas dword'ü üzerinden, **her şeridin çalıştığı** yerlerde değiştirilir
(grup bariyeri tüm thread'lerin oraya varmasını ister).

### İlk şerit okuması (`v_readfirstlane`)

Konsolun ilk etkin şeridi diğer yarıda olabilir. Ama okunan değer EXEC'teki
her şeritte aynıysa, hangi yarı okursa okusun sonuç aynıdır; takas gerekmez.
Tekdüzelik (uniformity) iki düzeyde izlenir:

- **Her şeritte aynı:** korumasız (EXEC'in tamamıyla çalışan) bir komutun
  skalerlerden, sabitlerden ve böyle değerlerden ürettiği değer.
- **Bir korumanın şeritlerinde aynı:** korumalı bir komut, EXEC dışındaki
  şeritlerde eski değeri bırakır; yazdığı değer yalnızca o korumanın
  şeritlerinde aynıdır. Aynı korumadaki bir `v_readfirstlane` için bu yeterlidir.
- **LDS okuması:** her şeridin aynı tuttuğu bir adresten `ds_read*`
  (şerit numarasını adrese katan `ds_read_addtid` hariç) her şeride aynı
  dword'ü verir.

Tipik örnek: grubun konsol dalgaları üzerinde dönen bir tarama döngüsü. EXEC,
skaler sayaç ile dalga numarasının eşitliğidir (tam bir konsol dalgası); döngü
içinde tekdüze adresten okunan bir toplam `v_readfirstlane` ile skalere alınır.

Kısayol: iki yarının LDS dword'ünü, biri onu değiştirmeden önce okuduğu
varsayılır (konsolun tek dalgası gibi); yarılar bariyerler arasında sıralı
değildir.

## LDS ve şerit dalları

Konsol bir dalganın şeritlerini adım adım çalıştırır; dalga içinde LDS
paylaşan kod bariyer koymaz. İki host dalgası bu sırayı ancak aralarında bir
grup bariyeriyle korur. Bariyer şerit dalının içine konamaz (her thread
varmalı), bu yüzden dalın bölgesine giriş ve çıkışta konur. Bir bölgenin
içinde hem okuma hem yazma varsa bariyer sırayı koruyamaz — **ama ancak ikisi
farklı yarıların şeritlerinden aynı baytlara ulaşabiliyorsa** sorun vardır:

| Durum | Neden güvenli |
|---|---|
| İkisini de tek thread çalıştırıyor | Bir thread kendi program sırasını izler. Tek-thread koşulu: tekdüze bir değerle thread numarasının eşitliği (`tid == 0`); thread numarası tekdüze miktarlarla toplama, çıkarma, xor, `kaydır+topla` ile taşınmış olabilir (hepsi birebir). Koruma bu koşulu gerektirmeli (BDD üzerinde gerektirme testi). |
| Aynı tekdüze tabandan örtüşmeyen baytlar | Her şerit aynı adresleri kullanır; adres kümeleri ayrık. |
| İki sabit tabandan (`v_mov` sabiti) örtüşmeyen baytlar | Adresler sayı olarak karşılaştırılır. |
| İkisi de atomik | Atomik işlemler birbirleriyle değişmelidir; HLSL bir sıra vaat etmez. |

İki yazma arasındaki çakışma ayrıca ele alınmaz (kısayol: aynı dword'e iki
yarıdan yazılmadığı varsayılır).

Kaçınılan yol: dalları "dalga boyunca" almak (dalın içine grup bariyeri
koymak) test oyununda GPU'yu kilitledi ve kart düştü (device removed). Kural
tabanlı yaklaşım hiç bariyer eklemez.

### Henüz kapsanmayanlar

- İki boyutlu gruplarda (ör. 8x8) tek-thread koşulu: `v0` ve `v1` ayrı
  koordinatlardır; ya iki eşitliğin birleşimi ya da düzleştirilmiş indeks
  tanınmalı.
- Konsol dalgası başına tek şerit (`v_mbcnt == 0`, ilk etkin şerit): çakışma
  yalnızca aynı konsol dalgasının iki yarısı arasında önemlidir; farklı konsol
  dalgaları zaten konsol kodundaki bariyerlerle sıralıdır.
- Şerit indeksli tabanlar (`tid * 4 + sabit`) için aralık analizi.

## Yarım-dalga yaklaşımı

Bazı programlar takası tüm şeritlerin çalışmadığı bir blokta ister: ör. HLSL
`WaveActiveAllTrue(koşul)`, konsol derleyicisinde `(koşul ∧ EXEC) xor EXEC`
maskesindeki şerit sayısının sıfır olup olmadığı testi olarak çıkar; ardından
şerit dallarında ilk-şerit okumaları ve indirgemeler gelir. Bariyer
konamadığı için kesin çeviri mümkün değildir.

Yaklaşık yol: her yarı kendi cevabını kullanır (dalga genelindeki sonuç iki
yarım-dalga sonucuna dönüşür). Bu tür testler çoğunlukla bir hızlandırmayı
seçer ("tüm şeritler tamamen gölgede ise süzmeyi atla"), sonuç her şerit için
geçerli kalır. Test oyununda kademeli gölge haritası ve ekran-uzayı izleme
programları bu yolla çalıştı; yedi dakikalık oynanışta GPU kilitlenmedi.
Kesin olmayan bir yol olduğu için kapatılabilir kalmalı ve hangi programların
ondan geçtiği günlükte görünmeli.
