use std::{cmp, sync::Arc};

use smol::lock::Mutex;

use crate::{RingbufferSpeedometer, Speedometer as _};

pub struct StatusStats {
    pub statuscode: String,
    start: std::time::Instant,
    pub pending: u32, // pending since start
    pub ring: RingbufferSpeedometer,
}

impl StatusStats {
    fn new(statuscode: String) -> Self {
        Self {
            statuscode,
            start: std::time::Instant::now(),
            pending: 0,
            ring: RingbufferSpeedometer::new(5),
        }
    }
    fn process(&mut self) {
        let elapsed = self.start.elapsed().as_millis() as u32;
        if elapsed == 0 {
            return;
        }
        self.ring.add_measurement(elapsed, self.pending);
        self.start = std::time::Instant::now();
        self.pending = 0;
    }
}

impl Ord for StatusStats {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.statuscode.cmp(&other.statuscode)
    }
}
impl PartialOrd for StatusStats {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for StatusStats {
    fn eq(&self, other: &Self) -> bool {
        self.statuscode == other.statuscode
    }
}
impl Eq for StatusStats {}

pub struct GroupStats {
    pub group: String,
    pub stats: Vec<StatusStats>,
    global_statuscodes: GlobalStatuscodes,
}
impl GroupStats {
    pub fn new(group: String, global_statuscodes: GlobalStatuscodes) -> Self {
        Self {
            group,
            stats: vec![],
            global_statuscodes,
        }
    }
    pub async fn get_or_create(&mut self, statuscode: String) -> &mut StatusStats {
        // we only max ~5 tags so looping is faster than a hashmap
        if let Some(index) = self.stats.iter().position(|x| x.statuscode == statuscode) {
            &mut self.stats[index]
        } else {
            let mut globalstate = self.global_statuscodes.lock().await;
            globalstate.push(statuscode.clone());
            globalstate.sort();
            globalstate.dedup();
            self.stats.push(StatusStats::new(statuscode));
            self.stats.sort();
            self.stats.last_mut().unwrap()
        }
    }
    pub fn process(&mut self) {
        for statusstats in self.stats.iter_mut() {
            statusstats.process();
        }
    }
    pub fn iter(&mut self) -> impl Iterator<Item = &StatusStats> {
        self.stats.iter()
    }
}

type GlobalStatuscodes = Arc<Mutex<Vec<String>>>;

pub struct GroupMap {
    pub stats: Vec<GroupStats>,
    pub shared_prefix: String,
    pub shared_suffix: String,
    global_statuscodes: GlobalStatuscodes,
}
impl GroupMap {
    pub fn new(global_statuscodes: GlobalStatuscodes) -> Self {
        Self {
            stats: vec![],
            shared_prefix: "".to_owned(),
            shared_suffix: "".to_owned(),
            global_statuscodes,
        }
    }
    pub fn get_or_create(&mut self, tag: String) -> &mut GroupStats {
        // we only expect a max of ~5 groups so looping is faster than a hashmap
        if let Some(index) = self.stats.iter().position(|x| x.group == tag) {
            &mut self.stats[index]
        } else {
            self.stats
                .push(GroupStats::new(tag, self.global_statuscodes.clone()));
            self.update_trimmed_tags();
            self.stats.last_mut().unwrap()
        }
    }

    pub fn len(&self) -> usize {
        self.stats.len()
    }

    #[allow(unused)]
    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }

    pub fn iter(&mut self) -> impl Iterator<Item = &GroupStats> {
        self.stats.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut GroupStats> {
        self.stats.iter_mut()
    }

    fn update_trimmed_tags(&mut self) {
        if self.stats.len() < 2 {
            return;
        }

        self.shared_prefix.clear();
        self.shared_suffix.clear();

        let mut max_tag_length = 0;

        // shared strings can never be longer than the any of the tags, so we'll
        // just use the first one to iterate over
        'outer: for i in 0..self.stats[0].group.len() {
            for tag in self.stats.iter() {
                max_tag_length = cmp::max(max_tag_length, tag.group.len()); // while we're here, let's also find the max tag length
                if tag.group.chars().nth(i) != self.stats[0].group.chars().nth(i) {
                    break 'outer;
                }
            }
            self.shared_prefix
                .push(self.stats[0].group.chars().nth(i).unwrap());
        }

        'outer: for i in 0..self.stats[0].group.len() {
            for tag in self.stats.iter() {
                if tag.group.chars().nth_back(i) != self.stats[0].group.chars().nth_back(i) {
                    break 'outer;
                }
            }
            self.shared_suffix
                .insert(0, self.stats[0].group.chars().nth_back(i).unwrap());
        }

        // we don't need to reduce the tags to nothing,
        // there's space on the screen for some text
        let text_left = max_tag_length - self.shared_prefix.len() - self.shared_suffix.len();
        if text_left < 8 {
            if max_tag_length < 8 {
                self.shared_prefix = "".to_owned();
                self.shared_suffix = "".to_owned();
            } else {
                // we do not alter the suffix: it's probably .log which we want to filter out
                let chars_to_preserve = 8 - text_left;
                let cut_from_prefix = cmp::max(0, self.shared_prefix.len() - chars_to_preserve);
                self.shared_prefix = self.shared_prefix[..cut_from_prefix].to_owned();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::collections::GlobalStatuscodes;

    #[test]
    fn test_tagmap_with_short_tags() {
        let mut tagmap = super::GroupMap::new(GlobalStatuscodes::default());
        assert!(tagmap.is_empty());
        assert_eq!(tagmap.len(), 0);

        let tag1 = tagmap.get_or_create("200".to_owned());
        assert_eq!(tag1.group, "200");
        assert_eq!(tagmap.len(), 1);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        let tag2 = tagmap.get_or_create("500".to_owned());
        assert_eq!(tag2.group, "500");
        assert_eq!(tagmap.len(), 2);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        tagmap.get_or_create("404".to_owned());
        assert_eq!(tagmap.len(), 3);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        // reuse tag
        tagmap.get_or_create("200".to_owned());
        assert_eq!(tagmap.len(), 3);
    }

    #[test]
    fn test_tagmap_with_long_tags() {
        let mut tagmap = super::GroupMap::new(GlobalStatuscodes::default());
        assert!(tagmap.is_empty());
        assert_eq!(tagmap.len(), 0);

        let tag1 =
            tagmap.get_or_create("/var/log/nginx/sites/customer_project_0/access.log".to_owned());
        assert_eq!(
            tag1.group,
            "/var/log/nginx/sites/customer_project_0/access.log"
        );
        assert_eq!(tagmap.len(), 1);
        assert_eq!(tagmap.shared_prefix, "");
        assert_eq!(tagmap.shared_suffix, "");

        let tag2 =
            tagmap.get_or_create("/var/log/nginx/sites/customer_project_1/access.log".to_owned());
        assert_eq!(
            tag2.group,
            "/var/log/nginx/sites/customer_project_1/access.log"
        );
        assert_eq!(tagmap.len(), 2);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/customer_p");
        assert_eq!(tagmap.shared_suffix, "/access.log");

        tagmap.get_or_create("/var/log/nginx/sites/customer_project_2/access.log".to_owned());
        assert_eq!(tagmap.len(), 3);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/customer_p");
        assert_eq!(tagmap.shared_suffix, "/access.log");

        // reuse tag
        tagmap.get_or_create("/var/log/nginx/sites/customer_project_1/access.log".to_owned());
        assert_eq!(tagmap.len(), 3);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/customer_p");
        assert_eq!(tagmap.shared_suffix, "/access.log");

        // like a "root" log file
        let tag5 = tagmap.get_or_create("/var/log/nginx/sites/access.log".to_owned());
        assert_eq!(tag5.group, "/var/log/nginx/sites/access.log");
        assert_eq!(tagmap.len(), 4);
        assert_eq!(tagmap.shared_prefix, "/var/log/nginx/sites/");
        assert_eq!(tagmap.shared_suffix, "/access.log");
    }
}
