//! Tests of the TTS text normaliser: table rows in Turkish and English for numbers,
//! ordinals, percentages, currencies, dates, times, phone numbers and abbreviations, the
//! per-chunk integration with the segmenter, and properties over arbitrary input (never panics,
//! terminates, bounded growth, idempotent on its own output for plain text).
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::borrow::Cow;

use brisvox_agent::normalize::{
    INLINE_CHUNK_CHARS, Normalizer, normalize, spell_cardinal, spell_digits, spell_ordinal,
};
use brisvox_agent::segment::{Language, Segmenter, SegmenterConfig};
use proptest::prelude::*;

const TR: Language = Language::Tr;
const EN: Language = Language::En;

/// `(input, spoken)` rows, Turkish.
const TR_ROWS: &[(&str, &str)] = &[
    // --- Integers and large numbers (the dot is the thousands separator) ---
    ("0", "sıfır"),
    ("7", "yedi"),
    ("10", "on"),
    ("21", "yirmi bir"),
    ("100", "yüz"),
    ("101", "yüz bir"),
    ("215", "iki yüz on beş"),
    ("1000", "bin"),
    ("1.000", "bin"),
    ("2000", "iki bin"),
    ("1.250", "bin iki yüz elli"),
    ("12.345", "on iki bin üç yüz kırk beş"),
    ("100.000", "yüz bin"),
    ("1.250.000", "bir milyon iki yüz elli bin"),
    ("2.000.000.000", "iki milyar"),
    ("1.000.000.000.000", "bir trilyon"),
    ("Toplam 1.000 kişi geldi.", "Toplam bin kişi geldi."),
    ("Yüzde 5 arttı", "Yüzde beş arttı"),
    // Numbers with a leading zero and over-long digit runs are read digit by digit.
    ("007", "sıfır sıfır yedi"),
    (
        "12345678901234567890",
        "bir iki üç dört beş altı yedi sekiz dokuz sıfır bir iki üç dört beş altı yedi sekiz dokuz sıfır",
    ),
    // --- Decimals (the comma is the decimal separator) ---
    ("3,5", "üç virgül beş"),
    ("0,05", "sıfır virgül sıfır beş"),
    ("2,25", "iki virgül yirmi beş"),
    ("3,14159", "üç virgül bir dört bir beş dokuz"),
    ("1.250,75", "bin iki yüz elli virgül yetmiş beş"),
    ("3.5", "üç virgül beş"),
    ("0.123", "sıfır virgül bir iki üç"),
    ("-5", "eksi beş"),
    ("Sıcaklık -12 derece", "Sıcaklık eksi on iki derece"),
    // --- Ordinals ---
    ("3. sınıf", "üçüncü sınıf"),
    ("1. kat", "birinci kat"),
    ("21. yüzyıl", "yirmi birinci yüzyıl"),
    ("100. yıl", "yüzüncü yıl"),
    ("4. cadde", "dördüncü cadde"),
    ("2. el araba", "ikinci el araba"),
    // A period before a capital is a sentence end, not an ordinal.
    ("Ben 3. Sonra gittik.", "Ben üç. Sonra gittik."),
    // --- Apostrophe suffixes stay attached to the spoken form ---
    ("15'inde", "on beşinde"),
    ("3'üncü", "üçüncü"),
    ("1990'larda", "bin dokuz yüz doksanlarda"),
    ("5’e kadar", "beşe kadar"),
    ("2026'da", "iki bin yirmi altıda"),
    // --- Percentages ---
    ("%15", "yüzde on beş"),
    ("15%", "yüzde on beş"),
    ("%3,5", "yüzde üç virgül beş"),
    ("Faiz %12'ye çıktı.", "Faiz yüzde on ikiye çıktı."),
    ("100%", "yüzde yüz"),
    // --- Currency ---
    ("150 TL", "yüz elli lira"),
    ("150 TRY", "yüz elli lira"),
    ("₺150", "yüz elli lira"),
    ("150₺", "yüz elli lira"),
    ("1.250,50 TL", "bin iki yüz elli lira elli kuruş"),
    ("150,5 TL", "yüz elli lira elli kuruş"),
    ("0,75 TL", "yetmiş beş kuruş"),
    ("20 $", "yirmi dolar"),
    ("$20", "yirmi dolar"),
    ("20 USD", "yirmi dolar"),
    ("€20", "yirmi avro"),
    ("20 EUR", "yirmi avro"),
    ("£5", "beş sterlin"),
    ("2,50 €", "iki avro elli sent"),
    ("150 TL'ye", "yüz elli lira"),
    ("Ücret 150 TL'dir.", "Ücret yüz elli lira."),
    ("₺1,5 milyon", "bir virgül beş milyon lira"),
    ("5 milyon TL", "beş milyon lira"),
    // --- Dates ---
    ("12.03.2026", "on iki Mart iki bin yirmi altı"),
    ("01.05.2026", "bir Mayıs iki bin yirmi altı"),
    ("2026-03-12", "on iki Mart iki bin yirmi altı"),
    ("12/03/2026", "on iki Mart iki bin yirmi altı"),
    ("31-12-1999", "otuz bir Aralık bin dokuz yüz doksan dokuz"),
    ("12 Mart 2026", "on iki Mart iki bin yirmi altı"),
    (
        "Toplantı 12.03.2026 tarihinde.",
        "Toplantı on iki Mart iki bin yirmi altı tarihinde.",
    ),
    ("12.03.2026'da", "on iki Mart iki bin yirmi altıda"),
    // An impossible date is not a date.
    (
        "32.13.2026",
        "otuz iki nokta on üç nokta iki bin yirmi altı",
    ),
    // Version numbers and addresses: each group is a number, joined with "nokta".
    ("sürüm 2.0.1", "sürüm iki nokta sıfır nokta bir"),
    (
        "192.168.1.1",
        "yüz doksan iki nokta yüz altmış sekiz nokta bir nokta bir",
    ),
    ("v2.0.1", "v2.0.1"),
    // --- Times ---
    ("14:30", "on dört otuz"),
    ("09:05", "dokuz sıfır beş"),
    ("9:05", "dokuz sıfır beş"),
    ("14:00", "on dört"),
    ("00:00", "sıfır sıfır"),
    ("23:59", "yirmi üç elli dokuz"),
    ("14:30:15", "on dört otuz on beş"),
    ("Saat 14:30'da gel.", "Saat on dört otuzda gel."),
    ("25:00", "yirmi beş:sıfır sıfır"),
    // --- Phone numbers ---
    (
        "0532 123 45 67",
        "sıfır beş üç iki, bir iki üç, dört beş, altı yedi",
    ),
    (
        "05321234567",
        "sıfır beş üç iki, bir iki üç, dört beş, altı yedi",
    ),
    (
        "+90 532 123 45 67",
        "artı dokuz sıfır, beş üç iki, bir iki üç, dört beş, altı yedi",
    ),
    (
        "+905321234567",
        "artı dokuz sıfır, beş üç iki, bir iki üç, dört beş, altı yedi",
    ),
    (
        "0 (212) 555 12 34",
        "sıfır, iki bir iki, beş beş beş, bir iki, üç dört",
    ),
    (
        "Beni 0532 123 45 67 numaradan ara.",
        "Beni sıfır beş üç iki, bir iki üç, dört beş, altı yedi numaradan ara.",
    ),
    // --- Abbreviations ---
    ("Dr. Ayşe Yılmaz", "Doktor Ayşe Yılmaz"),
    ("Prof. Dr. Ahmet Bey", "Profesör Doktor Ahmet Bey"),
    ("Öğr. Gör. Zeynep", "Öğretim Görevlisi Zeynep"),
    ("Sn. Müdür", "Sayın Müdür"),
    (
        "elma, armut vb. meyveler",
        "elma, armut ve benzeri meyveler",
    ),
    ("elma, armut vb.", "elma, armut ve benzeri."),
    (
        "kitap, defter vs. Sonra gittik",
        "kitap, defter vesaire. Sonra gittik",
    ),
    ("Atatürk Cad. No 5", "Atatürk Caddesi No beş"),
    ("Akdeniz Mah. 12. Sok.", "Akdeniz Mahallesi on ikinci Sokak"),
    (
        "Örnek A.Ş. genel müdürü",
        "Örnek Anonim Şirketi genel müdürü",
    ),
    ("5 dk. sonra", "beş dakika sonra"),
    ("30 sn. bekle", "otuz saniye bekle"),
    // --- Things that stay as they are ---
    ("", ""),
    ("Merhaba, nasılsınız?", "Merhaba, nasılsınız?"),
    ("A4 kağıt", "A4 kağıt"),
    ("mp3 dosyası", "mp3 dosyası"),
    ("5km yürüdük", "beş km yürüdük"),
    ("İstanbul'da ğüşiöç", "İstanbul'da ğüşiöç"),
    ("Tamam. Görüşürüz.", "Tamam. Görüşürüz."),
    ("%", "%"),
    ("$ işareti", "$ işareti"),
    ("1 + 2", "bir + iki"),
    // --- Review fixes: dotted clock times (a dot is a time only after `saat` or with a case
    // suffix; everywhere else it is a decimal point) ---
    ("Saat 14.30 gibi", "Saat on dört otuz gibi"),
    ("saat 09.30'da", "saat dokuz otuzda"),
    ("yarın 10.30'da görüşelim", "yarın on otuzda görüşelim"),
    ("saat 7.30'da", "saat yedi otuzda"),
    ("Saat 18.00", "Saat on sekiz"),
    ("Pi sayısı 3.14", "Pi sayısı üç virgül on dört"),
    ("Saat 25.30", "Saat yirmi beş virgül otuz"),
    // --- Currency: code before the amount, glued to it, with a scale word ---
    ("USD 20", "yirmi dolar"),
    ("TL 150", "yüz elli lira"),
    ("EUR 2,50", "iki avro elli sent"),
    ("150TL", "yüz elli lira"),
    ("20USD", "yirmi dolar"),
    ("US$20", "yirmi dolar"),
    ("US$ 5", "beş dolar"),
    ("5 milyar $", "beş milyar dolar"),
    ("2 milyon €", "iki milyon avro"),
    ("R$ 5", "R$ 5"),
    ("A$5", "A$5"),
    ("TL işareti", "lira işareti"),
    // --- Thousands grouped by a space or a no-break space ---
    ("100 000 TL", "yüz bin lira"),
    ("1 000 000", "bir milyon"),
    ("12\u{a0}500 kişi", "on iki bin beş yüz kişi"),
    ("1 250,50 TL", "bin iki yüz elli lira elli kuruş"),
    ("3 4 5", "üç dört beş"),
    ("2026 1000", "iki bin yirmi altı bin"),
    // --- Impossible calendar dates are not dates ---
    (
        "31.02.2026",
        "otuz bir nokta sıfır iki nokta iki bin yirmi altı",
    ),
    (
        "31.04.2026",
        "otuz bir nokta sıfır dört nokta iki bin yirmi altı",
    ),
    ("29.02.2024", "yirmi dokuz Şubat iki bin yirmi dört"),
    (
        "29.02.2026",
        "yirmi dokuz nokta sıfır iki nokta iki bin yirmi altı",
    ),
    // --- Ordinals: capitalised nouns, four-digit numbers ---
    ("5. Cadde", "beşinci Cadde"),
    ("3. Kat", "üçüncü Kat"),
    ("2. Sınıf", "ikinci Sınıf"),
    ("2026. yılında", "iki bin yirmi altıncı yılında"),
    ("Ben 5. Gün geldi.", "Ben beş. Gün geldi."),
    // --- Phone numbers: bare mobile, no leading zero, call-centre numbers ---
    (
        "Telefonum 5321234567",
        "Telefonum beş üç iki, bir iki üç, dört beş, altı yedi",
    ),
    (
        "532 123 45 67",
        "beş üç iki, bir iki üç, dört beş, altı yedi",
    ),
    ("444 12 34", "dört dört dört, bir iki, üç dört"),
    ("444 0 123", "dört dört dört, sıfır, bir iki üç"),
    ("444 1234", "dört dört dört, bir iki üç dört"),
    ("4441234", "dört dört dört, bir iki üç dört"),
    (
        "5 321 234 567",
        "beş milyar üç yüz yirmi bir milyon iki yüz otuz dört bin beş yüz altmış yedi",
    ),
    (
        "1234567890",
        "bir milyar iki yüz otuz dört milyon beş yüz altmış yedi bin sekiz yüz doksan",
    ),
    // --- Abbreviations that are also words are only expanded in an abbreviation context ---
    ("Ateşi yak.", "Ateşi yak."),
    ("Lambayı yak. Sonra gel", "Lambayı yak. Sonra gel"),
    ("Hemen bul.", "Hemen bul."),
    ("Anahtarı sok.", "Anahtarı sok."),
    ("Bul. Sonra gel", "Bul. Sonra gel"),
    ("Tel.", "Tel."),
    ("Av.", "Av."),
    ("Dr.", "Dr."),
    (
        "Tel. 0212 555 12 34",
        "Telefon sıfır iki bir iki, beş beş beş, bir iki, üç dört",
    ),
    ("Av. Mehmet Yıldız", "Avukat Mehmet Yıldız"),
    ("Gazi Bul. No 5", "Gazi Bulvarı No beş"),
    ("Gazi Bul. 12. Sok.", "Gazi Bulvarı on ikinci Sokak"),
    ("örn. elma", "örneğin elma"),
    // --- Review fixes: a hyphen between two times or dates is the dash of a range, not part of
    // the first number (the dash itself stays in the text) ---
    (
        "Pazartesi-Cuma 09:00-18:00",
        "Pazartesi-Cuma dokuz-on sekiz",
    ),
    ("09:00-18:00", "dokuz-on sekiz"),
    ("09:30-17:45", "dokuz otuz-on yedi kırk beş"),
    (
        "09:00:30-18:00:30",
        "dokuz sıfır sıfır otuz-on sekiz sıfır sıfır otuz",
    ),
    ("saat 09.00-17.00", "saat dokuz-on yedi"),
    (
        "Saat 9.30-17.45 arası",
        "Saat dokuz otuz-on yedi kırk beş arası",
    ),
    // Without `saat` or a case suffix a dotted pair is still a decimal, range or not.
    (
        "17.00-18.00",
        "on yedi virgül sıfır sıfır-on sekiz virgül sıfır sıfır",
    ),
    (
        "12.03.2026-15.03.2026",
        "on iki Mart iki bin yirmi altı-on beş Mart iki bin yirmi altı",
    ),
    (
        "12/03/2026-15/03/2026",
        "on iki Mart iki bin yirmi altı-on beş Mart iki bin yirmi altı",
    ),
    (
        "Tarih 12.03.2026 - 15.03.2026 arası",
        "Tarih on iki Mart iki bin yirmi altı - on beş Mart iki bin yirmi altı arası",
    ),
    // A date that itself uses `-` still continues with another `-number`: not a date.
    ("12-03-2026-15", "on iki-sıfır üç-iki bin yirmi altı-on beş"),
    // --- Review fixes: rules named in docs/TURKISH.md that had no row ---
    // A time needs a two-digit minute: 16:9 is an aspect ratio, not 16:09.
    ("16:9", "on altı:dokuz"),
    ("16:9 ekran", "on altı:dokuz ekran"),
    // A dotted pair is a time only after `saat` or with a time case suffix.
    ("Fiyat 12.50'ye", "Fiyat on iki elliye"),
    ("7.30", "yedi virgül otuz"),
    ("10.30 TL", "on lira otuz kuruş"),
    // The comma list without spaces reads as a decimal first.
    ("1,2,3", "bir virgül iki,üç"),
    // The Turkish ordinal takes at most four digits.
    ("1234. sınıf", "bin iki yüz otuz dördüncü sınıf"),
    ("12345. sınıf", "on iki bin üç yüz kırk beş. sınıf"),
    // A context abbreviation after a lowercase word (or alone) is a word, not an abbreviation.
    ("ateşi Sok.", "ateşi Sok."),
    ("hemen Bul.", "hemen Bul."),
    ("lütfen Tel.", "lütfen Tel."),
    // ... and after a capitalised word it is one.
    ("Atatürk Sok.", "Atatürk Sokak"),
    ("Gazi Bul.", "Gazi Bulvarı"),
    // Company and era abbreviations keep their period only at the very end of the chunk.
    ("Örnek A.Ş.", "Örnek Anonim Şirketi."),
    ("Örnek T.C.", "Örnek Türkiye Cumhuriyeti."),
    ("Örnek M.Ö.", "Örnek Milattan önce."),
    ("Örnek M.S.", "Örnek Milattan sonra."),
    ("A.Ş. yetkilisi", "Anonim Şirketi yetkilisi"),
    ("T.C. Hükümeti", "Türkiye Cumhuriyeti Hükümeti"),
    ("M.Ö. 500 yılında", "Milattan önce beş yüz yılında"),
    ("M.S. 500 yılında", "Milattan sonra beş yüz yılında"),
    // --- Review fixes: one row per abbreviation of the table ---
    ("Doç. Dr. Ali", "Doçent Doktor Ali"),
    ("Yrd. Doç. Ali", "Yardımcı Doçent Ali"),
    ("Yrd. Doç. Dr. Ali", "Yardımcı Doçent Doktor Ali"),
    ("Uzm. Ali", "Uzman Ali"),
    ("Müh. Ali", "Mühendis Ali"),
    ("Op. Ali", "Operatör Ali"),
    ("Prof. Ali", "Profesör Ali"),
    ("Apt. 5", "Apartmanı beş"),
    ("Örnek Ltd. Şti. geldi", "Örnek Limited Şirketi geldi"),
    ("Ltd.", "Limited"),
    ("Şti.", "Şirketi"),
    ("bkz. sayfa 5", "bakınız sayfa beş"),
    ("bkz.", "bakınız"),
    ("armut vd.", "armut ve diğerleri."),
    ("armut vd. Sonra gel", "armut ve diğerleri. Sonra gel"),
    ("armut vd. sonra", "armut ve diğerleri sonra"),
    ("Atatürk Mah.", "Atatürk Mahallesi"),
    ("Atatürk Cad.", "Atatürk Caddesi"),
    ("Sn. Ali", "Sayın Ali"),
    ("Av. Ali", "Avukat Ali"),
    ("Tel. 5", "Telefon beş"),
    // A title needs a capitalised name after it.
    ("Doç. ali", "Doç. ali"),
    ("Uzm. 5", "Uzm. beş"),
];

/// `(input, spoken)` rows, English.
const EN_ROWS: &[(&str, &str)] = &[
    // --- Integers and decimals (the comma is the thousands separator) ---
    ("0", "zero"),
    ("13", "thirteen"),
    ("21", "twenty-one"),
    ("100", "one hundred"),
    ("101", "one hundred one"),
    ("1,000", "one thousand"),
    ("1,250,000", "one million two hundred fifty thousand"),
    ("2000", "two thousand"),
    ("3.5", "three point five"),
    ("3.14", "three point one four"),
    (
        "1,250.75",
        "one thousand two hundred fifty point seven five",
    ),
    ("-5", "minus five"),
    ("It is -12 outside", "It is minus twelve outside"),
    ("1,2,3", "one,two,three"),
    // --- Ordinals ---
    ("3rd", "third"),
    ("1st place", "first place"),
    ("22nd", "twenty-second"),
    ("100th", "one hundredth"),
    ("12th floor", "twelfth floor"),
    ("20th", "twentieth"),
    ("5th", "fifth"),
    // --- Percent ---
    ("15%", "fifteen percent"),
    ("3.5%", "three point five percent"),
    ("%15", "fifteen percent"),
    // --- Currency ---
    ("$20", "twenty dollars"),
    ("$1", "one dollar"),
    ("$1.50", "one dollar fifty cents"),
    ("$0.99", "ninety-nine cents"),
    (
        "$1,250,000",
        "one million two hundred fifty thousand dollars",
    ),
    ("20 USD", "twenty dollars"),
    ("€20", "twenty euros"),
    ("£5.01", "five pounds one penny"),
    ("150 TRY", "one hundred fifty Turkish lira"),
    ("$1.5 million", "one point five million dollars"),
    // --- Dates ---
    ("March 12, 2026", "March twelfth, twenty twenty-six"),
    ("March 12", "March twelfth"),
    ("12 March 2026", "the twelfth of March, twenty twenty-six"),
    ("the 3rd of May", "the third of May"),
    ("2026-03-12", "March twelfth, twenty twenty-six"),
    ("12.03.2026", "March twelfth, twenty twenty-six"),
    ("03/12/2026", "March twelfth, twenty twenty-six"),
    ("25/12/2026", "December twenty-fifth, twenty twenty-six"),
    ("July 4, 1905", "July fourth, nineteen oh five"),
    ("May 2026", "May two thousand twenty-six"),
    // --- Times ---
    ("14:30", "fourteen thirty"),
    ("9:05", "nine oh five"),
    ("9:00", "nine o'clock"),
    ("14:00", "fourteen hundred"),
    ("00:00", "midnight"),
    ("9:30 PM", "nine thirty p m"),
    ("9 PM", "nine PM"),
    ("12:00 am", "twelve a m"),
    ("Meet at 14:30.", "Meet at fourteen thirty."),
    // --- Phone numbers ---
    (
        "555-123-4567",
        "five five five, one two three, four five six seven",
    ),
    (
        "+1 555 123 4567",
        "plus one, five five five, one two three, four five six seven",
    ),
    (
        "(555) 123-4567",
        "five five five, one two three, four five six seven",
    ),
    (
        "+44 20 7946 0958",
        "plus four four, two zero, seven nine four six, zero nine five eight",
    ),
    // --- Abbreviations ---
    ("Mr. Smith", "Mister Smith"),
    ("Dr. Jones and Prof. Lee", "Doctor Jones and Professor Lee"),
    ("cats, dogs, etc.", "cats, dogs, et cetera."),
    ("cats, etc. and dogs", "cats, et cetera and dogs"),
    ("red e.g. apples", "red for example apples"),
    ("Acme Inc. announced", "Acme Incorporated announced"),
    ("A vs. B", "A versus B"),
    // --- Version numbers stay or are read group by group ---
    ("v2.0.1", "v2.0.1"),
    ("version 2.0.1", "version two dot zero dot one"),
    // --- Things that stay as they are ---
    ("", ""),
    ("Hello, how are you?", "Hello, how are you?"),
    ("A4 paper", "A4 paper"),
    ("COVID19", "COVID19"),
    ("5km", "five km"),
    ("Hi. Bye.", "Hi. Bye."),
    ("US$ sign", "US$ sign"),
    // --- Review fixes: years and decades outside a full date ---
    ("In 1999, we won.", "In nineteen ninety-nine, we won."),
    ("since 2015", "since twenty fifteen"),
    ("in 2005", "in two thousand five"),
    ("the 1990s", "the nineteen nineties"),
    ("the 1900s", "the nineteen hundreds"),
    ("the 2000s", "the two thousands"),
    ("the 2010s", "the twenty tens"),
    ("the 1990's", "the nineteen nineties"),
    (
        "1995 people",
        "one thousand nine hundred ninety-five people",
    ),
    ("in 1999 dollars", "in nineteen ninety-nine dollars"),
    (
        "in 1999 USD",
        "in one thousand nine hundred ninety-nine dollars",
    ),
    ("1999", "one thousand nine hundred ninety-nine"),
    // --- Review fixes: currency code before or glued to the amount, symbols after letters ---
    ("USD 20", "twenty dollars"),
    ("EUR 5", "five euros"),
    ("150TRY", "one hundred fifty Turkish lira"),
    ("US$20", "twenty dollars"),
    ("US$5", "five dollars"),
    ("R$ 5", "R$ 5"),
    ("5 billion $", "five billion dollars"),
    // --- Large English ordinals ---
    ("1000th place", "one thousandth place"),
    ("2026th", "two thousand twenty-sixth"),
    // --- Space grouped thousands, impossible dates, titles at the end of a sentence ---
    ("100 000", "one hundred thousand"),
    (
        "31.02.2026",
        "thirty-one dot zero two dot two thousand twenty-six",
    ),
    ("Elm Dr.", "Elm Dr."),
    ("Dr. Smith", "Doctor Smith"),
    ("Call Dr. who", "Call Dr. who"),
    // --- Review fixes: ranges, abbreviations ---
    ("09:00-18:00", "nine o'clock-eighteen hundred"),
    (
        "12.03.2026-15.03.2026",
        "March twelfth, twenty twenty-six-March fifteenth, twenty twenty-six",
    ),
    // One row per abbreviation of the table, with the negative context where one is needed.
    ("Mrs. Smith", "Missus Smith"),
    ("Ms. Smith", "Miz Smith"),
    ("Call Mrs. who", "Call Mrs. who"),
    ("Call Ms. who", "Call Ms. who"),
    ("Prof. Lee", "Professor Lee"),
    ("Jr.", "Junior."),
    ("Sr.", "Senior."),
    ("John Smith Jr.", "John Smith Junior."),
    ("John Jr. came", "John Junior came"),
    ("Acme Ltd.", "Acme Limited."),
    ("Acme Ltd. came", "Acme Limited came"),
    ("Acme Inc.", "Acme Incorporated."),
    ("approx. 5", "approximately five"),
    ("approx.", "approx."),
    ("approx. ", "approx. "),
    ("i.e. apples", "that is apples"),
    ("i.e.", "i.e."),
    ("e.g.", "e.g."),
    ("vs.", "vs."),
    ("etc.", "et cetera."),
    ("etc. Then", "et cetera. Then"),
];

#[test]
fn turkish_table() {
    assert!(TR_ROWS.len() >= 60, "{}", TR_ROWS.len());
    for (input, spoken) in TR_ROWS {
        assert_eq!(normalize(input, TR), *spoken, "tr: {input:?}");
    }
}

#[test]
fn english_table() {
    assert!(EN_ROWS.len() >= 40, "{}", EN_ROWS.len());
    for (input, spoken) in EN_ROWS {
        assert_eq!(normalize(input, EN), *spoken, "en: {input:?}");
    }
}

#[test]
fn the_table_has_at_least_sixty_rows_in_total() {
    assert!(TR_ROWS.len() + EN_ROWS.len() >= 60);
}

#[test]
fn the_normalizer_wraps_the_function() {
    let tr = Normalizer::new(TR);
    assert_eq!(tr.language(), TR);
    assert_eq!(tr.normalize("%15"), "yüzde on beş");
    assert_eq!(Normalizer::default().language(), Language::En);
    assert_eq!(Normalizer::default().normalize("%15"), "fifteen percent");
    let mut chunk = String::from("Toplam 1.250 TL");
    tr.normalize_in_place(&mut chunk);
    assert_eq!(chunk, "Toplam bin iki yüz elli lira");
    // Nothing to do: no allocation, the string is untouched.
    let mut plain = String::from("Merhaba dünya");
    let before = plain.as_ptr();
    tr.normalize_in_place(&mut plain);
    assert_eq!(plain, "Merhaba dünya");
    assert_eq!(plain.as_ptr(), before);
}

#[test]
fn text_without_anything_to_rewrite_is_borrowed() {
    for text in [
        "",
        "Merhaba, nasılsınız?",
        "Tamam",
        "yes, I see",
        "çok güzel ğüşiöç",
    ] {
        assert!(matches!(normalize(text, TR), Cow::Borrowed(_)), "{text:?}");
        assert!(matches!(normalize(text, EN), Cow::Borrowed(_)), "{text:?}");
    }
    // Something that looks rewritable but comes out identical is borrowed too.
    assert!(matches!(normalize("Hi. Bye.", EN), Cow::Borrowed(_)));
}

#[test]
fn spelling_functions_cover_the_table_edges() {
    assert_eq!(spell_cardinal(0, TR), "sıfır");
    assert_eq!(spell_cardinal(1_000_000, TR), "bir milyon");
    assert_eq!(spell_cardinal(1_001, TR), "bin bir");
    assert_eq!(
        spell_cardinal(1_000_000_000_000_000_000, TR),
        "bir kentilyon"
    );
    assert_eq!(
        spell_cardinal(u64::MAX, TR),
        "on sekiz kentilyon dört yüz kırk altı katrilyon yedi yüz kırk dört trilyon \
         yetmiş üç milyar yedi yüz dokuz milyon beş yüz elli bir bin altı yüz on beş"
    );
    assert_eq!(spell_cardinal(0, EN), "zero");
    assert_eq!(
        spell_cardinal(u64::MAX, EN),
        "eighteen quintillion four hundred forty-six quadrillion seven hundred forty-four \
         trillion seventy-three billion seven hundred nine million five hundred fifty-one \
         thousand six hundred fifteen"
    );
    // Turkish ordinals follow vowel harmony on the last word.
    for (n, word) in [
        (1, "birinci"),
        (2, "ikinci"),
        (3, "üçüncü"),
        (4, "dördüncü"),
        (5, "beşinci"),
        (6, "altıncı"),
        (7, "yedinci"),
        (8, "sekizinci"),
        (9, "dokuzuncu"),
        (10, "onuncu"),
        (20, "yirminci"),
        (30, "otuzuncu"),
        (40, "kırkıncı"),
        (50, "ellinci"),
        (60, "altmışıncı"),
        (70, "yetmişinci"),
        (80, "sekseninci"),
        (90, "doksanıncı"),
        (100, "yüzüncü"),
        (1000, "bininci"),
        (1_000_000, "bir milyonuncu"),
        (1_000_000_000, "bir milyarıncı"),
        (0, "sıfırıncı"),
    ] {
        assert_eq!(spell_ordinal(n, TR), word, "tr ordinal {n}");
    }
    assert_eq!(spell_ordinal(0, EN), "zeroth");
    assert_eq!(spell_ordinal(1000, EN), "one thousandth");
    assert_eq!(spell_ordinal(12, EN), "twelfth");
    assert_eq!(spell_digits("a1b0-5", TR), "bir sıfır beş");
    assert_eq!(spell_digits("", EN), "");
}

#[test]
fn turkish_output_keeps_the_real_turkish_letters() {
    // The dotted and dotless i, the cedilla, breve and umlauts must come out as typed.
    let out = normalize("%5 ve 6 ve 9 ve 40 ve 90 ve 3,5", TR);
    assert_eq!(
        out,
        "yüzde beş ve altı ve dokuz ve kırk ve doksan ve üç virgül beş"
    );
    for c in ['ı', 'ü', 'ş'] {
        assert!(out.contains(c), "{c}");
    }
    assert!(normalize("15. gün", TR).contains("on beşinci"));
    assert!(normalize("Mart", TR).chars().all(|c| c.is_alphabetic()));
    assert_eq!(normalize("10. yıl", TR), "onuncu yıl");
    assert_eq!(normalize("8. sınıf", TR), "sekizinci sınıf");
    assert_eq!(normalize("6. kat", TR), "altıncı kat");
}

#[test]
fn works_per_segmenter_chunk_without_changing_the_segmenter() {
    // The segmenter keeps `3. sınıf` and `1.250` whole (its own rules); the normaliser then
    // rewrites each chunk. The segmenter output itself is the unchanged text.
    let text = "Oğlum 3. sınıf öğrencisi. Fatura 1.250,50 TL, ödeme 12.03.2026 tarihinde. \
                Saat 14:30'da Dr. Ayşe arayacak.";
    let mut seg = Segmenter::new(SegmenterConfig::new(TR));
    let mut chunks = Vec::new();
    for piece in text.split_inclusive(' ') {
        seg.push(piece, &mut chunks);
    }
    seg.finish(&mut chunks);
    assert_eq!(chunks.join(" "), text, "segmenter output is untouched");
    let norm = Normalizer::new(TR);
    for chunk in &mut chunks {
        norm.normalize_in_place(chunk);
    }
    let spoken = chunks.join(" ");
    assert!(spoken.contains("üçüncü sınıf"), "{spoken}");
    assert!(
        spoken.contains("bin iki yüz elli lira elli kuruş"),
        "{spoken}"
    );
    assert!(
        spoken.contains("on iki Mart iki bin yirmi altı"),
        "{spoken}"
    );
    assert!(spoken.contains("on dört otuzda Doktor Ayşe"), "{spoken}");
    assert!(!spoken.chars().any(|c| c.is_ascii_digit()), "{spoken}");
}

/// Feeds `text` to a fresh segmenter `step` chars at a time and returns the chunks.
fn segment_streaming(text: &str, lang: Language, step: usize) -> Vec<String> {
    let mut seg = Segmenter::new(SegmenterConfig::new(lang));
    let mut chunks = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    for piece in chars.chunks(step.max(1)) {
        let piece: String = piece.iter().collect();
        seg.push(&piece, &mut chunks);
    }
    seg.finish(&mut chunks);
    chunks
}

#[test]
fn a_capitalised_ordinal_noun_split_by_the_segmenter_is_a_documented_limit() {
    // The segmenter ends a sentence at `digit. Capital` (unchanged by the Turkish quality pass), so
    // `5. Cadde` is cut when the cut is otherwise allowed. The normaliser sees `5.` at the end
    // of a chunk, which is a sentence end, and keeps it as a cardinal. Inside one chunk the
    // capitalised noun works (table rows `5. Cadde`, `2. Sınıf`). docs/TURKISH.md, "Limits".
    let chunks = segment_streaming(
        "Oğlum Ali evde kalıyor ve kızım 5. Cadde üzerinde oturuyor.",
        TR,
        1,
    );
    assert!(chunks.len() > 1, "{chunks:?}");
    let norm = Normalizer::new(TR);
    let first = norm.normalize(&chunks[0]);
    assert!(first.ends_with("beş."), "{first}");
}

#[test]
fn chunk_boundaries_never_change_the_spoken_text() {
    // The segmenter feeds the TTS stage chunk by chunk, however the LLM tokens arrive. For the
    // constructs below (ordinal + noun, number + unit, date + suffix, title + name, address
    // abbreviations, dotted times) the spoken text of the chunks must equal the spoken text of
    // the whole message: no cut may fall inside a construct.
    let tr = "Oğlum 3. sınıf öğrencisi ve kızım Atatürk Cad. üzerinde oturuyor. Fatura 1.250,50 TL, \
              ödeme 12.03.2026 tarihinde yapılacak. Saat 14:30'da Dr. Ayşe arayacak, yoksa saat 15.45 \
              gibi Prof. Dr. Ahmet Bey arar. Adres Gazi Bul. 12. sokak No 5, telefon \
              0532 123 45 67. Faiz %15'e çıktı vb. Sonra 100 000 TL ödendi, yaklaşık 3,5 saat sürdü. \
              Toplantı 2026. yılında 3. kat 2. daire. Başka bir şey var mı, vs. Evet.";
    let en = "Mr. Smith lives on the 3rd floor and pays $1,250.50 a month. The meeting is on \
              March 12, 2026 at 9:30 PM, call Dr. Jones at 555-123-4567 soon. In 1999 the 1990s \
              began, with 15% growth and 1000th place for Acme Inc. Then they left, etc. Done.";
    for (text, lang) in [(tr, TR), (en, EN)] {
        let whole = normalize(text, lang).into_owned();
        for step in [1usize, 2, 3, 5, 8, 13, 40, 1000] {
            let chunks = segment_streaming(text, lang, step);
            if step <= 40 {
                assert!(chunks.len() > 1, "{lang:?} step {step}: {chunks:?}");
            }
            assert_eq!(
                chunks.join(" "),
                text,
                "{lang:?} step {step}: segmenter changed text"
            );
            let norm = Normalizer::new(lang);
            let spoken: Vec<String> = chunks
                .iter()
                .map(|c| norm.normalize(c).into_owned())
                .collect();
            assert_eq!(
                spoken.join(" "),
                whole,
                "{lang:?} step {step}: chunks {chunks:?}"
            );
        }
    }
}

/// The best (smallest) time of `runs` runs of `f`, in nanoseconds; the library has no clock
/// (injected `Clock` rule), a measurement belongs in the test.
fn best_ns(runs: usize, mut f: impl FnMut()) -> u128 {
    (0..runs)
        .map(|_| {
            #[allow(clippy::disallowed_methods)]
            let start = std::time::Instant::now();
            f();
            #[allow(clippy::disallowed_methods)]
            let elapsed = start.elapsed();
            elapsed.as_nanos()
        })
        .min()
        .unwrap_or(0)
}

#[test]
fn cost_grows_linearly_with_the_input() {
    // 16 times the input must cost about 16 times as much; an accidental rescan (quadratic)
    // would cost 256 times. The bound of 64 leaves room for cache effects and a noisy machine.
    // Shapes chosen to exercise the scanners that look ahead: digit runs with separators,
    // dotted groups, parentheses (phone), space groups, currency, abbreviations.
    let shapes = [
        "1 2 3 ",
        "1.1.1.",
        "(1)",
        "1.000.000.5.5.",
        "12 345 678 ",
        "1,2,3,",
        "Dr. Av. Tel. 1. ",
        "5 TL $ % ",
        "12:3 4:56 ",
        // Long runs without separators, and looking back/ahead over letters and periods.
        "abcdefghij1 ",
        "Dr.Dr.Dr.",
        "a.b.c.d.e.",
        "........",
        "x9",
        "5.",
        "0,0.0:0-",
        "Sn. Sok. ",
        "A.Ş. T.C. ",
    ];
    for lang in [TR, EN] {
        for shape in shapes {
            let small = shape.repeat(400);
            let large = shape.repeat(6_400);
            let t_small = best_ns(5, || {
                std::hint::black_box(normalize(std::hint::black_box(&small), lang));
            })
            .max(1);
            let t_large = best_ns(5, || {
                std::hint::black_box(normalize(std::hint::black_box(&large), lang));
            });
            assert!(
                t_large < t_small * 64,
                "{lang:?} {shape:?}: small {t_small} ns, 16x input {t_large} ns"
            );
        }
    }
}

#[test]
fn the_inline_budget_covers_the_default_segmenter_chunk() {
    for lang in [TR, EN] {
        assert!(INLINE_CHUNK_CHARS >= SegmenterConfig::new(lang).max_chunk_chars);
    }
}

/// The mean cost of normalising `chunk` in microseconds (best of five batches of `batch` calls).
fn mean_us(chunk: &str, lang: Language, batch: u32) -> u128 {
    best_ns(5, || {
        for _ in 0..batch {
            std::hint::black_box(normalize(std::hint::black_box(chunk), lang));
        }
    }) / u128::from(batch)
        / 1_000
}

#[test]
fn a_typical_chunk_costs_microseconds_not_milliseconds() {
    // An absolute guard for the 330-char mixed Turkish chunk that docs/LATENCY.md measures at
    // 8-10 us in release. The bound is 2 ms in release (200 times the measurement: it catches a
    // constant-factor blow-up such as a `Vec<char>` rebuilt per token, which the ratio test
    // below cannot) and 20 ms in a debug build, which is slower by an order of magnitude.
    let chunk =
        "Toplam 1.250.000 TL, %15 indirim, 12.03.2026 saat 14:30, Dr. Ayşe 0532 123 45 67. "
            .repeat(4);
    assert!(chunk.chars().count() >= 320, "{}", chunk.chars().count());
    let limit_us: u128 = if cfg!(debug_assertions) {
        20_000
    } else {
        2_000
    };
    let cost = mean_us(&chunk, TR, 20);
    assert!(cost < limit_us, "{cost} us per chunk, limit {limit_us} us");
}

#[test]
fn a_digit_dense_inline_chunk_stays_within_the_worker_budget() {
    // docs/LATENCY.md: the 50 us rule for work on a tokio worker holds for chunks up to
    // INLINE_CHUNK_CHARS even digit-dense (measured about 10 us in release for 256 chars). The
    // bound is 10 times the 50 us rule in release (500 us: generous for a noisy machine, still a
    // 50x gap to a quadratic path) and 10 ms in a debug build.
    let limit_us: u128 = if cfg!(debug_assertions) { 10_000 } else { 500 };
    for lang in [TR, EN] {
        for unit in [
            "1 2 3 ",
            "12.50 TL, ",
            "%5 14:30 ",
            "0532 123 45 67 ",
            "1.000.000 ",
        ] {
            let chunk: String = unit
                .repeat(INLINE_CHUNK_CHARS)
                .chars()
                .take(INLINE_CHUNK_CHARS)
                .collect();
            let cost = mean_us(&chunk, lang, 20);
            assert!(
                cost < limit_us,
                "{lang:?} {unit:?}: {cost} us for {INLINE_CHUNK_CHARS} chars, limit {limit_us} us"
            );
        }
    }
}

/// Strings built from the tokens the normaliser cares about, to hit its branches densely.
fn tokenish() -> impl Strategy<Value = String> {
    let atoms = prop::sample::select(vec![
        "0",
        "1",
        "3",
        "7",
        "12",
        "2026",
        "1.250",
        "3,5",
        "3.5",
        ".",
        ",",
        ":",
        "-",
        "/",
        "'",
        "’",
        "%",
        "₺",
        "$",
        "€",
        "£",
        "TL",
        "USD",
        "Dr.",
        "vb.",
        "etc.",
        "Mr.",
        "March",
        "of",
        "st",
        "th",
        "PM",
        " ",
        " ",
        " ",
        "a",
        "ı",
        "İ",
        "ş",
        "+",
        "(",
        ")",
        "\n",
        "ab",
        "A.Ş.",
        "14:30",
        "12.03.2026",
        "0532",
        "555-123-4567",
        "milyon",
        "million",
        "999999999999999999999",
    ]);
    prop::collection::vec(atoms, 0..40).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn never_panics_on_arbitrary_text(text in ".{0,200}") {
        for lang in [TR, EN] {
            let _ = normalize(&text, lang);
        }
    }

    #[test]
    fn never_panics_on_token_soup(text in tokenish()) {
        for lang in [TR, EN] {
            let out = normalize(&text, lang);
            // Growth is bounded: every digit becomes at most ~9 chars (digit words) plus joiners.
            prop_assert!(out.len() <= text.len().saturating_mul(24).saturating_add(64),
                "{} -> {}", text.len(), out.len());
        }
    }

    #[test]
    fn digits_never_survive_in_plain_numbers(n in 0u64..=u64::MAX, lang in prop::sample::select(vec![TR, EN])) {
        let text = n.to_string();
        let out = normalize(&text, lang);
        prop_assert!(!out.chars().any(|c| c.is_ascii_digit()), "{n} -> {out}");
        prop_assert!(!out.is_empty());
    }

    #[test]
    // Currency codes (`TL`, `USD`, ...) are spoken even alone, so they are not plain words.
    fn plain_words_are_untouched(text in "[a-zçğıöşüA-ZÇĞİÖŞÜ ,!?;]{0,80}"
        .prop_filter("no currency code", |t| !["TL", "TRY", "USD", "EUR", "GBP"].iter().any(|c| t.contains(c)))) {
        for lang in [TR, EN] {
            prop_assert!(matches!(normalize(&text, lang), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn spoken_numbers_need_no_second_pass(n in 0u64..1_000_000_000_000, lang in prop::sample::select(vec![TR, EN])) {
        let once = normalize(&format!("{n}"), lang).into_owned();
        let twice = normalize(&once, lang).into_owned();
        prop_assert_eq!(once, twice);
    }
}
