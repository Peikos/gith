+++
title = "Gith"
+++

# Gith
`gith` is een alternatief voor *GitHub Classroom* met als doel het bieden van (een relevante subset van) dezelfde functionaliteit, maar zonder de hele *tech stack* over te nemen. GitHub Classroom was, in mijn gebruik, een goede tool om administratie van studenten-repo's te automatisren, maar nodeloos complex. Het is als tool gebouwd op een tool die weer op een andere tool rust, en al deze lagen voegen complexiteit en abstracties toe. Dit leidt tot een systeem dat de *principle of least surprise* dikwijls niet respecteert. Met het heengaan van GitHub classroom is `gith` een alternatief dat dichter bij de basis begint (`git` en `ssh`) en daar de minimale tooling opzet om het beoogde resultaat te bereiken. Bij de implementatie staan de volgende uitgangspunten centraal:

- Hou het zo simpel mogelijk (maar niet simpeler)
- Doe liever *een* ding goed dan *alle* dingen matig
- Het bovengenoemde *principle of least surprise*

## Docenten

Alle communicatie met de tool verloopt via SSH. Hier wordt SSH niet gebruikt om in te loggen op een interactieve shell (bash en dergelijke), maar om directe commando's naar de server te sturen. Om een commando uit te voeren dien je geauthoriseerd te zijn. De server weet wie iemand is (en daarmee welke rechten iemand heeft) doordat er bij het maken van de verbinding een SSH key wordt gebruikt.

### Classroom maken

De basiseenheid van organisatie is de classroom. Er is geen "organisatie" zoals bij GitHub classroom het geval was: deze heeft voor het concept "vakken met daarbinnen repositories" geen waarde en was in GitHub Classroom vooral aanwezig omdat dit deel is van de onderliggende GitHub API waar Classroom bovenop was gebouwd.

Ik kan als docent een nieuwe classroom aanmaken met het volgende commando. Na afloop is er een lege classroom gecreëerd met de gebruiker als teacher. 

```bash
ssh gith.athena.peikos.net classroom create ai-s2 "Fundamentals of AI"
```

In dit voorbeeld is `ai-s2` de slug (gebruikt voor URLs en identificatie, moet uniek zijn en mag geen spaties bevatten), en is "Fundamentals of AI" de human-readable naam.

### Opdrachten maken

Ik kan een template opdracht aan een classroom toevoegen met het `add-template` commando. Ik geef de slug van de gewenste classroom om de opdracht in aan te maken, en een tweede slug voor de naam van de opdracht. Deze is op zijn minst uniek binnen een classroom.

```bash
ssh gith.athena.peikos.net classroom add-template ai-s2 api-requests
```

Na het uitvoeren van dit commando is er voor docenten een repo beschikbaar gekomen. Ik kan hier vervolgens een bestaande repo naartoe pushen:

```bash
cd api-requests
git remote add gith gith.athena.peikos.net:ai-s2/api-requests.git
git push -u gith main
```

De `gith` server is hier als tweede remote toegevoegd, ervan uitgaande dat de (bestaande) repo al op bijvoorbeeld GitHub staat en de remote `origin` daarheen verwijst.


Alternatief kan ik de lege repo clonen, en van hieruit werken:
```bash
git clone gith.athena.peikos.net:ai-s2/api-requests.git
cd api-requests
touch README.md # Maak een (leeg) README bestand aan
git add README.md
git commit -m "Initial commit"
git push -u origin main
```

### Studenten (docenten/TAs) toevoegen via token

Om mensen aan een bestaande classroom toe te voegen kan ik een invite maken worden. Dit gaat wederom op basis van classroom slug. Daarnaast moet een rol worden toegekend uit de set { `student`, `ta`, `teacher` }. Het resultaat is een base-64 token die met de invitees gedeeld kan worden om in te loggen, waarbij de rechten waarmee de token is aangemaakt worden toegekend aan de SSH key van de nieuwe teacher/ta/student.

- Een `teacher` kan nieuwe classrooms, invites en opdrachten maken; daarnaast heeft die push-toegang tot de templates en read-toegang tot de studenten-repositories.
- Een `ta` heeft enkel read-toegang tot de repositories van studenten. 
- Een student heeft volle toegang tot diens eigen repositories. De student heeft automatisch een persoonlijke kopie van elke repository die door een `teacher` als template is toegevoegd.

```bash
ssh gith.athena.peikos.net classroom invite ai-s2 student
# => 4i0pFRCvfG6vOLDCtuT4cJhhQK8aAMYT
```

### Docent/TA commando's

Als docent kan ik een overzicht krijgen van de studenten, templates en bijbehorende URLs in classroom:

```bash
ssh gith.athena.peikos.net classroom list ai-s2
```

Om een studenten-repository te clonen gebruik ik een URL gebaseerd op de slugs van de classroom, opdracht, en student. De slug van de student wordt door hen zelf bepaald tijdens de registratie met de token; dit is de naam die zij invoeren zonder eventuele spaties.

```bash
git clone gith.athena.peikos.net:ai-s2/api-requests/JorisHeemkskerk.git
```

Om een opdracht voor meerdere studenten in bulk te downloaden kan ik een `tar` opvragen. Hier worden alleen actieve studenten in meegenomen.

```bash
ssh gith.athena.peikos.net classroom download ai-s2 api-requests > ai-s2-api-requests.tar.gz
```

Een student is (wanneer deze een invite accepteert) automatisch actief. Zodra een student een vak heeft afgerond kan ik deze als docent deactiveren. Bij een niet-actieve student wordt geen werk weggegooid, maar komt deze niet meer in de bulk download / klassenlijst voor. Daarnaast heeft die geen push rechten meer naar de repo, maar kan deze het eigen werk nog wel downloaden (pull / clone).

```bash
ssh gith.athena.peikos.net classroom deactivate ai-s2 JorisHeemskerk 
```

De filosofie achter deze aanpak is tweevoudig: enerzijds garandeert dit het bewaren van studentenwerk voor mogelijke audits.

Anderzijds neemt dit de noodzaak weg om voor iedere semester-iteratie een hele nieuwe classroom op te tuigen. Dit zou moeten leiden tot een overzichtelijker geheel, waarin altijd duidelijk is wat waar staat en er niet steeds nieuwe template repositories in het leven hoeven te worden geroepen (met als nadeel dat deze de synchronisatie verliezen, en verbeteringen van jaar op jaar weer verloren gaan als de verkeerde template wordt gekopieerd.).

## Studenten

Vanuit de student kant ziet de ervaring er vergelijkbaar maar simpeler uit. De student registreert zich met een via de docent (Canvas) verkregen token voor een classroom, en kan dan automatisch aan alle opdrachten daarbinnen deelnemen. Deze registratie vindt dus &eacute;&eacute;n keer per semester plaats (en kan bij herkansing worden overgeslagen). De enige benodigdheid van de student-kant is SSH (standaard op Mac en Linux, op Windows standaard meegeleverd met `git` via **Git Bash**) en een key (te maken met `ssh-keygen`):

```bash
ssh gith.athena.peikos.net register "$TOKEN" "Joris Heemskerk"
```

Dit is het enige commando waarvoor de student een andere tool dan het `git` commando *moet* gebruiken, hierna verloopt het pullen en pushen van code via standaard `git` commando's. Optioneel kan de student (net als docenten) via `ssh` een overicht van hun repo's opvragen met `list`:

```bash
ssh gith.athena.peikos.net list
```


### Clonen en pushen

Template repo's zijn automatisch voor alle studenten beschikbaar via een voorspelbare URL gebaseerd op de slug van de classroom en opdracht. Het is niet nodig hier de studentnaam in te gebruiken, omdat het systeem op basis van de SSH key al weet met wie het te maken heeft. Dit levert een enkele URL op die op Canvas kan worden opgenomen op de pagina van een opdracht.

```bash
git clone gith.athena.peikos.net:ai-s2/api-requests.git
cd api-requests
# commits...
git push origin main
```

Wanneer ik als docent push naar de template wordt dit automatisch meegenomen voor studenten die nog niet aan de opdracht zijn begonnen. Als de student al werk heeft ingediend wanneer ik als docent een fix push, dan levert dit mogelijke conflicten op, dus dit wordt nadat een student is begonnen niet meer automatisch op de `main` branch meegenomen. In plaats daarvan is er een tweede `upstream` branch die een student kan binnenhalen met `git fetch` gevolgd door `git merge origin/upstream`.

## Groepsprojecten
Op dit moment nog niet meegenomen, toekomstige implementatie afhankelijk van animo. Dit zou een extra laag in het datamodel toevoegen (en daarmee instructies voor student en docent compliceren), en is mogelijk in strid met kerndoelen "hou het zo simpel mogelijk" en "doe liever een ding goed dan alle dingen matig".

### Overweging
Hier wel [SourceHut](https://sr.ht/)/[GitLab](https://about.gitlab.com/)/[GitHub](https://github.com/)/[Forgejo](https://forgejo.org/) voor gebruiken. Dit sluit aan op leerdoelen om een industry standard te gebruiken. Fundamenteel verschil tussen "tooling om code templating en inlevering te ondersteunen" en "ICT project draaien in realistische omgeving".

## Grasduinen
Als jij als collega het systeem wil verkennen / testen, vraag dan [Brian van der Bijl](mailto:brian.vanderbijl@hu.nl) om een teacher invite. Met deze invite kom je terecht in de `gith` classroom, die als een soort van lobby fungeert. Maak vanuit hier gerust een test-classroom aan en voeg daar templates aan toe. Het is prima mogelijk om jezelf (ook) als student of TA toe te voegen met een tweede ssh-key. `ssh-keygen` stelt je in staat extra keys te generen, en met de `-i` parameter van `ssh` kun je een alternatieve key kiezen.

## Server Admin
De stappen in deze sectie zijn eenmalig nodig, en niet van toepassing bij bestaande installatie.

### Initialisatie

De eerste initialisatie vindt plaats via de command line; zodra de server draait is deze via SSH te benaderen om configuratiestappen uit te voeren en voor het serven van de Git repositories.

```bash
# Eenmalig
gith admin init
gith -- admin add-teacher --name "Brian van der Bijl" --key "$(cat ~/.ssh/id_ed25519.pub)"

# Starten server (kan idealiter in een `systemd` of vergelijkbaar systeem gemanaged worden - start at boot).
gith server --host 0.0.0.0 --port 22
```

Nu de server draait kan de rest via SSH gebeuren. Op dit moment is er slechts een geauthoriseerde gebruiker, die tijdens de initialisatie is aangemaakt. Via SSH kunnen classrooms en users worden aangemaakt en rechten worden toegekend. Alle authenticatie en authorisatie gebeurt of basis van SSH keys; password login staat uit.

De default data directory is `~/.local/share/gith`.

